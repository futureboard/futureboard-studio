//! Floating MIDI editor window — edits the shared [`Timeline`] via a second
//! [`PianoRoll`] entity (no duplicated clip/note state).

use std::sync::Arc;

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Styled, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::piano_roll::PianoRoll;
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::timeline_state::ClipType;
use crate::components::title_bar::external_window_titlebar;
use crate::components::virtual_keyboard::VirtualKeyboardPanel;
use crate::theme::Colors;

pub const MIDI_EDITOR_WINDOW_WIDTH: f32 = 960.0;
pub const MIDI_EDITOR_WINDOW_HEIGHT: f32 = 560.0;
pub const MIDI_EDITOR_WINDOW_MIN_WIDTH: f32 = 900.0;
pub const MIDI_EDITOR_WINDOW_MIN_HEIGHT: f32 = 500.0;

/// Which MIDI clip the floating editor is bound to (mirrors timeline selection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiEditorTarget {
    pub track_id: String,
    pub clip_id: String,
}

pub(crate) fn midi_editor_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("FUTUREBOARD_MIDI_EDITOR_DEBUG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

pub(crate) fn midi_editor_debug(message: &str) {
    if midi_editor_debug_enabled() {
        eprintln!("[MIDI Editor] {message}");
    }
}

pub struct MidiEditorWindow {
    timeline: Entity<Timeline>,
    piano_roll: Entity<PianoRoll>,
    /// Shared session-level virtual-keyboard service. This window only forwards
    /// key events into it; it never owns musical-typing state. Musical typing
    /// works here exactly when the keyboard panel is shown in the main window.
    virtual_keyboard: Entity<VirtualKeyboardPanel>,
    /// Last clip we successfully rendered — used for the "clip deleted" empty state.
    last_clip_id: Option<String>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    dispatch_command: Arc<dyn Fn(&'static str, &mut App) + Send + Sync>,
    /// Runs the studio keymap's MIDI Editor scope (and the global keys) for a
    /// key pressed here. Returns whether a command took the key.
    dispatch_key: MidiEditorKeyCb,
    focus_handle: FocusHandle,
}

/// Resolves and runs a key pressed in the floating MIDI editor.
pub type MidiEditorKeyCb = Arc<dyn Fn(&KeyDownEvent, &mut App) -> bool + Send + Sync>;

impl MidiEditorWindow {
    pub fn new(
        timeline: Entity<Timeline>,
        piano_roll: Entity<PianoRoll>,
        virtual_keyboard: Entity<VirtualKeyboardPanel>,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
        dispatch_command: Arc<dyn Fn(&'static str, &mut App) + Send + Sync>,
        dispatch_key: MidiEditorKeyCb,
        cx: &mut Context<Self>,
    ) -> Self {
        // The window's actions live in the editor's footer, so the window
        // needs no second status row of its own.
        let export = {
            let dispatch_command = dispatch_command.clone();
            Arc::new(move |_window: &mut Window, cx: &mut App| {
                // The editor always shows the selected clip, which is exactly
                // what the command resolves, so no id has to be threaded
                // through the &'static str command channel.
                (dispatch_command)("midi:export-clip", cx);
            }) as Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>
        };
        let dock = {
            let dispatch_command = dispatch_command.clone();
            Arc::new(move |_window: &mut Window, cx: &mut App| {
                (dispatch_command)("editor:open-bottom", cx);
            }) as Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>
        };
        piano_roll.update(cx, |roll, _cx| {
            roll.set_window_actions(Some(export), Some(dock));
        });
        Self {
            timeline,
            piano_roll,
            virtual_keyboard,
            last_clip_id: None,
            on_close,
            dispatch_command,
            dispatch_key,
            focus_handle: cx.focus_handle(),
        }
    }

    fn title_for_clip(&self, cx: &Context<Self>) -> String {
        let tl = self.timeline.read(cx);
        let clip_id = tl.state.selection.selected_clip_ids.first().cloned();
        let Some(clip_id) = clip_id else {
            return "MIDI Editor".to_string();
        };
        let name = tl
            .state
            .find_clip(&clip_id)
            .map(|(_, c)| c.name.as_str())
            .unwrap_or("MIDI clip");
        format!("MIDI Editor — {name}")
    }

    fn current_midi_clip(&self, cx: &Context<Self>) -> Option<(String, String)> {
        let tl = self.timeline.read(cx);
        let clip_id = tl.state.selection.selected_clip_ids.first()?.clone();
        let (track, clip) = tl.state.find_clip(&clip_id)?;
        if !matches!(clip.clip_type, ClipType::Midi { .. }) {
            return None;
        }
        Some((track.id.clone(), clip_id))
    }

    fn clip_still_exists(&self, cx: &Context<Self>, clip_id: &str) -> bool {
        self.timeline
            .read(cx)
            .state
            .find_clip(clip_id)
            .is_some_and(|(_, c)| matches!(c.clip_type, ClipType::Midi { .. }))
    }

    fn should_route_to_piano_roll(event: &KeyDownEvent) -> bool {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        let command = mods.control || mods.platform;
        if command && !mods.alt && !mods.function {
            return matches!(
                key,
                "a" | "A" | "c" | "C" | "x" | "X" | "v" | "V" | "d" | "D"
            );
        }
        !mods.control
            && !mods.platform
            && !mods.alt
            && !mods.function
            && matches!(key, "delete" | "backspace")
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Forward to the shared virtual-keyboard service FIRST so musical typing
        // (including Space = sustain) behaves the same here as in the main window
        // when the keyboard panel is shown. The service decides whether to handle
        // or ignore; if it consumes the key, nothing else in this window acts on
        // it. Held repeats are forwarded too so the service can drop them without
        // them leaking to transport. Updating the (separate) keyboard entity from
        // inside this window's lease is safe — the service's sink re-enters
        // StudioLayout, never this window.
        let window_id = window.window_handle().window_id();
        let mods = event.keystroke.modifiers;
        let command_modifier = mods.control || mods.alt || mods.platform || mods.function;
        if !event.is_held {
            crate::components::transport_key::forget_space_key_up();
        }
        let vkbd_consumed = self.virtual_keyboard.update(cx, |keyboard, cx| {
            keyboard.handle_key_down(
                window_id,
                event.keystroke.key.as_str(),
                command_modifier,
                event.is_held,
                false,
                cx,
            )
        });
        if vkbd_consumed {
            cx.stop_propagation();
            return;
        }

        if event.is_held {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "space" && !mods.control && !mods.alt && !mods.platform && !mods.function {
            // Claim the key-up as well, or GPUI turns it into a click on whatever
            // this window focused last (see `transport_key::claim_space_key_up`).
            crate::components::transport_key::claim_space_key_up();
            window.prevent_default();
            cx.stop_propagation();
            midi_editor_debug("command dispatch transport:play-pause");
            (self.dispatch_command)("transport:play-pause", cx);
            return;
        }
        // Every other key goes to the keymap first, in the MIDI Editor scope:
        // Q quantizes, the arrows nudge and transpose, R records — the same
        // keys as the docked editor. A key the keymap does not bind falls
        // through to the piano roll's own editing keys below.
        if (self.dispatch_key)(event, cx) {
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if (mods.control || mods.platform) && !mods.alt && !mods.function {
            match key {
                "e" | "E" => {
                    cx.stop_propagation();
                    // Already in floating editor — no-op refocus handled by layout.
                }
                _ => {}
            }
        }
        if Self::should_route_to_piano_roll(event) {
            cx.stop_propagation();
            let _ = self
                .piano_roll
                .update(cx, |roll, cx| roll.handle_key_event(event, window, cx));
        }
    }
}

impl Render for MidiEditorWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.title_for_clip(cx);

        let body: gpui::AnyElement = match self.current_midi_clip(cx) {
            Some((_track_id, clip_id)) => {
                self.last_clip_id = Some(clip_id);
                self.piano_roll.clone().into_any_element()
            }
            None if self
                .last_clip_id
                .as_ref()
                .is_some_and(|id| !self.clip_still_exists(cx, id)) =>
            {
                midi_editor_debug("target clip missing");
                empty_state(
                    "MIDI clip no longer exists",
                    "Close this window or select another MIDI clip in the arrangement.",
                )
            }
            None => empty_state(
                "No MIDI clip selected",
                "Select or create a MIDI clip to edit.",
            ),
        };

        let on_close = self.on_close.clone();
        let target = cx.entity().clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            // Keep focus on a 0×0 child — not the root. Root `track_focus` adds a
            // full-window hitbox that wins over `WindowControlArea::Drag` on Windows
            // (see layout.rs main window comment).
            .capture_key_down(move |event, window, cx| {
                let _ = target.update(cx, |this, cx| this.on_key(event, window, cx));
            })
            // Forward key-up to the shared keyboard service so notes started by
            // musical typing in this window release. Updates the keyboard entity
            // directly (not this window), mirroring the main window's key path.
            .capture_key_up({
                let virtual_keyboard = self.virtual_keyboard.clone();
                move |event, window: &mut Window, cx| {
                    let window_id = window.window_handle().window_id();
                    let _ = virtual_keyboard.update(cx, |keyboard, cx| {
                        keyboard.handle_key_up(window_id, event.keystroke.key.as_str(), cx)
                    });
                    // Swallow the transport key's own key-up so it does not also
                    // press whatever control holds focus in this window.
                    if crate::components::transport_key::take_space_key_up_claim() {
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar(
                title.as_str(),
                "midi-editor-window-close",
                move |window, cx| {
                    midi_editor_debug("close window");
                    on_close(window, cx);
                    window.remove_window();
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .bg(Colors::surface_base())
                    .child(body),
            )
    }
}

fn empty_state(title: &str, hint: &str) -> gpui::AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(Colors::text_muted())
                .child(hint.to_string()),
        )
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
pub fn open_midi_editor_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    timeline: Entity<Timeline>,
    piano_roll: Entity<PianoRoll>,
    virtual_keyboard: Entity<VirtualKeyboardPanel>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    dispatch_command: Arc<dyn Fn(&'static str, &mut App) + Send + Sync>,
    dispatch_key: MidiEditorKeyCb,
    cx: &mut App,
) -> Result<WindowHandle<MidiEditorWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(px(MIDI_EDITOR_WINDOW_WIDTH), px(MIDI_EDITOR_WINDOW_HEIGHT)),
        cx,
    );

    // The MIDI editor is a top-level tool window, not an owned dialog. Using
    // dialog options here leaves `dialog_parenting` enabled before changing
    // the kind to Floating, which can make GPUI/Win32 tear down the window
    // during creation instead of returning a normal window handle.
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(MIDI_EDITOR_WINDOW_MIN_WIDTH),
        px(MIDI_EDITOR_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| {
            MidiEditorWindow::new(
                timeline,
                piano_roll,
                virtual_keyboard,
                on_close,
                dispatch_command,
                dispatch_key,
                cx,
            )
        })
    })
    .map_err(|e| e.to_string())
}

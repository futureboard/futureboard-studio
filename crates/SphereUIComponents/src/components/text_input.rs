use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, point, px, relative, App, Bounds, ClipboardItem, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, GlobalElementId, InteractiveElement, IntoElement,
    KeyDownEvent, LayoutId, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Role,
    ShapedLine, StatefulInteractiveElement, Style, Styled, TextRun, UTF16Selection, Window,
};

use crate::components::context_menu::ContextMenuEntry;
use crate::theme::Colors;

/// Generate a window/view's [`gpui::EntityInputHandler`] by routing every
/// platform IME call to one owned [`TextInputState`] field. This is the single
/// place the 8-method delegation lives, so any text-hosting window opts into
/// systemwide IME (CJK/Thai composition + caret-anchored candidate window) by
/// naming its input field — no hand-written, drift-prone per-window impl. The
/// view must own `field: TextInputState` and pass `cx.entity()` as the
/// `ime_target` to [`text_field_with_callbacks_and_ime`]. Mirrors the proven
/// AddTrack bridge. Raw `handle_key_with_clipboard` may coexist: GPUI suppresses
/// key dispatch for keystrokes the platform IME consumes, so text never doubles.
///
/// ```ignore
/// crate::impl_single_input_window_ime!(PluginManagerWindow, search_input);
/// ```
#[macro_export]
macro_rules! impl_single_input_window_ime {
    ($view:ty, $field:ident) => {
        impl ::gpui::EntityInputHandler for $view {
            fn text_for_range(
                &mut self,
                range: ::std::ops::Range<usize>,
                actual_range: &mut ::std::option::Option<::std::ops::Range<usize>>,
                _window: &mut ::gpui::Window,
                _cx: &mut ::gpui::Context<Self>,
            ) -> ::std::option::Option<::std::string::String> {
                self.$field.text_for_utf16_range(range, actual_range)
            }

            fn selected_text_range(
                &mut self,
                ignore_disabled_input: bool,
                _window: &mut ::gpui::Window,
                _cx: &mut ::gpui::Context<Self>,
            ) -> ::std::option::Option<::gpui::UTF16Selection> {
                self.$field.selected_text_range_utf16(ignore_disabled_input)
            }

            fn marked_text_range(
                &self,
                _window: &mut ::gpui::Window,
                _cx: &mut ::gpui::Context<Self>,
            ) -> ::std::option::Option<::std::ops::Range<usize>> {
                self.$field.marked_text_range_utf16()
            }

            fn unmark_text(
                &mut self,
                _window: &mut ::gpui::Window,
                cx: &mut ::gpui::Context<Self>,
            ) {
                self.$field.unmark_text();
                cx.notify();
            }

            fn replace_text_in_range(
                &mut self,
                range: ::std::option::Option<::std::ops::Range<usize>>,
                text: &str,
                _window: &mut ::gpui::Window,
                cx: &mut ::gpui::Context<Self>,
            ) {
                self.$field.replace_text_in_utf16_range(range, text);
                cx.notify();
            }

            fn replace_and_mark_text_in_range(
                &mut self,
                range: ::std::option::Option<::std::ops::Range<usize>>,
                new_text: &str,
                new_selected_range: ::std::option::Option<::std::ops::Range<usize>>,
                _window: &mut ::gpui::Window,
                cx: &mut ::gpui::Context<Self>,
            ) {
                self.$field.replace_and_mark_text_in_utf16_range(
                    range,
                    new_text,
                    new_selected_range,
                );
                cx.notify();
            }

            fn bounds_for_range(
                &mut self,
                range_utf16: ::std::ops::Range<usize>,
                bounds: ::gpui::Bounds<::gpui::Pixels>,
                _window: &mut ::gpui::Window,
                _cx: &mut ::gpui::Context<Self>,
            ) -> ::std::option::Option<::gpui::Bounds<::gpui::Pixels>> {
                self.$field.bounds_for_utf16_range(range_utf16, bounds)
            }

            fn character_index_for_point(
                &mut self,
                _point: ::gpui::Point<::gpui::Pixels>,
                _window: &mut ::gpui::Window,
                _cx: &mut ::gpui::Context<Self>,
            ) -> ::std::option::Option<usize> {
                None
            }
        }
    };
}

pub const TEXT_INPUT_CUT: &str = "text-input:cut";
pub const TEXT_INPUT_COPY: &str = "text-input:copy";
pub const TEXT_INPUT_PASTE: &str = "text-input:paste";
pub const TEXT_INPUT_SELECT_ALL: &str = "text-input:select-all";
const TEXT_INPUT_PAD_X: f32 = 9.0;
const TEXT_INPUT_CHAR_W: f32 = 7.0;

pub type TextInputContextCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;
/// Applies a `TEXT_INPUT_*` command, or closes the menu when passed `None`.
pub type TextInputContextCommandCb = Arc<dyn Fn(Option<&str>, &mut Window, &mut App) + 'static>;

/// Everything the built-in Cut/Copy/Paste menu needs, captured at the moment it
/// is summoned.
///
/// The viewport and clipboard state are snapshotted here rather than read while
/// rendering because the render path has neither a `Window` nor an `App` — and
/// right-click time is also exactly when both are accurate.
#[derive(Debug, Clone, Copy)]
pub struct TextContextMenuAnchor {
    pub x: f32,
    pub y: f32,
    pub viewport_width: f32,
    pub viewport_height: f32,
    pub clipboard_has_text: bool,
}
pub type TextInputMouseCb = Arc<dyn Fn(&TextInputMouseEvent, &mut Window, &mut App) + 'static>;

#[derive(Clone, Default)]
pub struct TextInputCallbacks {
    pub on_context_menu: Option<TextInputContextCb>,
    pub on_mouse: Option<TextInputMouseCb>,
    /// Set by [`bind_mouse_selection`] so the field can draw and drive its own
    /// context menu. Owners that render their own menu (the studio shell, the
    /// Add Track / Preferences / plugin-picker windows) leave this `None`.
    pub on_context_command: Option<TextInputContextCommandCb>,
}

#[derive(Debug, Clone, Copy)]
pub enum TextInputMousePhase {
    Down,
    Drag,
    Up,
}

#[derive(Debug, Clone, Copy)]
pub struct TextInputMouseEvent {
    pub phase: TextInputMousePhase,
    /// Grapheme/char index under the pointer, resolved from the shaped line.
    pub index: usize,
    /// Whether Shift was held (extend the existing selection).
    pub extend: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputAction {
    Consumed,
    Submit,
    Cancel,
    Pass,
}

/// A cursor position, expressed as a `char` index into the committed value.
///
/// Char indices (not byte offsets) are the canonical cursor space for
/// [`TextInputState`]: they never split a UTF-8 byte sequence, and the UTF-16
/// bridge / grapheme helpers convert in and out of this space. Movement always
/// lands on a grapheme-cluster boundary, so a `TextCursor` is also always a safe
/// slice point for rendering.
pub type TextCursor = usize;

/// Snapshot of the active selection in committed-`char`-index space.
///
/// `anchor == cursor` means an empty selection (just a caret). Use
/// [`TextSelection::range`] for the normalized `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: TextCursor,
    pub cursor: TextCursor,
}

impl TextSelection {
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// Normalized `start..end` range (always `start <= end`).
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }
}

/// Active IME composition (pre-edit) state, in committed-`char`-index space.
///
/// While composing, the pre-edit text is held inside the committed value but
/// marked by `range`; it must not be treated as final until
/// [`TextInputEvent::CompositionCommit`]. `cursor` is the caret within the
/// pre-edit, used to anchor the candidate window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImeCompositionState {
    pub text: String,
    pub range: Range<usize>,
    pub cursor: TextCursor,
}

/// Clipboard/selection edit commands that require an [`App`] (for the system
/// clipboard) to apply. Routed through [`TextInputState::apply_command`].
#[derive(Debug, Clone)]
pub enum TextEditCommand {
    Copy,
    Cut,
    Paste(String),
    SelectAll,
    ReplaceSelection(String),
}

/// The single, IME-safe vocabulary every editable text field speaks.
///
/// Platform key/IME events are translated into these; [`TextInputState`] applies
/// them with Unicode- and grapheme-correct semantics. Insertion never comes from
/// a raw `KeyDown`/`key_char`: real text arrives as [`TextInputEvent::InsertText`]
/// (committed key text) or through the composition lifecycle
/// (`CompositionStart` → `CompositionUpdate` → `CompositionCommit`/`Cancel`),
/// which mirrors the OS [`gpui::EntityInputHandler`] path.
#[derive(Debug, Clone)]
pub enum TextInputEvent {
    InsertText(String),
    CompositionStart,
    CompositionUpdate { text: String, cursor: usize },
    CompositionCommit(String),
    CompositionCancel,
    DeleteBackward,
    DeleteForward,
    MoveLeft,
    MoveRight,
    MoveWordLeft,
    MoveWordRight,
    MoveToStart,
    MoveToEnd,
    SelectAll,
    ReplaceSelection(String),
    Copy,
    Cut,
    Paste(String),
}

/// Pure, focus-handle-free text editing model: the committed value, caret,
/// selection, and IME composition, plus every Unicode-correct edit operation.
///
/// Holding no GPUI handle keeps the editing core unit-testable in isolation.
/// [`TextInputState`] wraps it (adding focus, mouse, and rendering concerns) and
/// derefs to it, so existing callers keep using `state.value`, `state.cursor`,
/// `state.handle_key_with_clipboard(..)`, etc. unchanged.
#[derive(Clone, Default)]
pub struct TextEditBuffer {
    pub value: String,
    pub cursor: usize,
    selection_anchor: Option<usize>,
    pub disabled: bool,
    pub read_only: bool,
    /// Open Cut/Copy/Paste menu, when the field draws its own (see
    /// [`bind_mouse_selection`]). `None` when closed, or when the owning window
    /// renders the menu itself.
    pub context_menu: Option<TextContextMenuAnchor>,
    pub is_password: bool,
    marked_range: Option<Range<usize>>,
}

#[derive(Clone)]
pub struct TextInputState {
    pub element_id: &'static str,
    pub focus_handle: FocusHandle,
    pub placeholder: Option<String>,
    accessible_label: Option<String>,
    pub buffer: TextEditBuffer,
    /// When true, a mouse-down outside this field (while it is focused) releases
    /// keyboard focus via `window.blur()`. Off by default so modal dialogs keep
    /// their field focused; the always-visible main-window fields opt in.
    pub blur_on_click_outside: bool,
    mouse_selecting: bool,
}

impl std::ops::Deref for TextInputState {
    type Target = TextEditBuffer;
    fn deref(&self) -> &TextEditBuffer {
        &self.buffer
    }
}

impl std::ops::DerefMut for TextInputState {
    fn deref_mut(&mut self) -> &mut TextEditBuffer {
        &mut self.buffer
    }
}

impl TextInputState {
    pub fn new(element_id: &'static str, focus_handle: FocusHandle) -> Self {
        Self {
            element_id,
            focus_handle,
            placeholder: None,
            accessible_label: None,
            buffer: TextEditBuffer::default(),
            blur_on_click_outside: false,
            mouse_selecting: false,
        }
    }

    pub fn with_placeholder(mut self, p: impl Into<String>) -> Self {
        self.placeholder = Some(p.into());
        self
    }

    /// Give assistive technology a stable, human-readable name for this field.
    /// When omitted, the placeholder is used, then the element id as a fallback.
    pub fn with_accessible_label(mut self, label: impl Into<String>) -> Self {
        self.accessible_label = Some(label.into());
        self
    }

    fn accessible_label(&self) -> String {
        self.accessible_label
            .clone()
            .or_else(|| {
                self.placeholder
                    .clone()
                    .filter(|label| !label.trim().is_empty())
            })
            .unwrap_or_else(|| self.element_id.replace(['-', '_'], " "))
    }

    /// Opt into releasing keyboard focus when the user clicks outside this field.
    pub fn blur_on_outside_click(mut self, enabled: bool) -> Self {
        self.blur_on_click_outside = enabled;
        self
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// Place the caret / begin a drag selection at the given grapheme index
    /// (resolved from the shaped line). `extend` (Shift) keeps the existing
    /// anchor and moves only the caret.
    pub fn handle_mouse_down(&mut self, index: usize, extend: bool) {
        if extend {
            self.buffer.select_to(index);
        } else {
            self.buffer.place_cursor(index);
        }
        self.mouse_selecting = true;
    }

    /// Extend the active drag selection to the given grapheme index.
    pub fn handle_mouse_drag(&mut self, index: usize) {
        if !self.mouse_selecting {
            return;
        }
        self.buffer.select_to(index);
    }

    pub fn handle_mouse_up(&mut self) {
        self.mouse_selecting = false;
        self.buffer.collapse_empty_selection();
    }
}

impl TextEditBuffer {
    pub fn set_value(&mut self, v: impl Into<String>) {
        self.value = v.into();
        self.cursor = self.char_count();
        self.clear_selection();
        self.unmark_text();
    }

    pub fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    pub fn set_password(&mut self, is_password: bool) {
        self.is_password = is_password;
    }

    pub fn select_all(&mut self) {
        let count = self.char_count();
        if count > 0 {
            self.selection_anchor = Some(0);
            self.cursor = count;
        }
    }

    /// Place the caret at `index` (clamped), collapsing any selection. Drives a
    /// plain mouse-down. Index is in committed-char space.
    pub fn place_cursor(&mut self, index: usize) {
        if self.disabled {
            return;
        }
        let idx = index.min(self.char_count());
        // Keep an active IME range until the platform commits or cancels it.
        // Clearing it here can leave pre-edit text behind when a click is followed
        // by `replace_text_in_range(None, ..)` from the IME.
        self.cursor = idx;
        self.selection_anchor = Some(idx);
    }

    /// Move the caret to `index` (clamped) while keeping the selection anchor —
    /// the mouse-drag / Shift-click selection primitive.
    pub fn select_to(&mut self, index: usize) {
        if self.disabled {
            return;
        }
        let idx = index.min(self.char_count());
        self.move_cursor_to(idx, true);
    }

    /// Drop a zero-width selection left by a plain click (so it renders as a caret,
    /// not an empty highlight). Mouse-up uses this.
    pub fn collapse_empty_selection(&mut self) {
        if self.selection_anchor == Some(self.cursor) {
            self.clear_selection();
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn unmark_text(&mut self) {
        self.marked_range = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    pub fn can_cut(&self) -> bool {
        !self.disabled && !self.read_only && self.has_selection()
    }

    pub fn can_copy(&self) -> bool {
        !self.disabled && self.has_selection()
    }

    pub fn can_paste(&self) -> bool {
        !self.disabled && !self.read_only
    }

    pub fn can_select_all(&self) -> bool {
        !self.disabled && !self.value.is_empty()
    }

    pub fn handle_key(&mut self, event: &KeyDownEvent) -> TextInputAction {
        self.handle_key_with_clipboard(event, None)
    }

    /// Key handling for a field WITHOUT an OS IME bridge. Printable characters are
    /// inserted here from the platform-resolved `key_char` (covers Thai, accents,
    /// AltGr, and dead-key results), since no `replace_text_in_range` will arrive.
    pub fn handle_key_with_clipboard(
        &mut self,
        event: &KeyDownEvent,
        cx: Option<&mut App>,
    ) -> TextInputAction {
        self.handle_key_inner(event, cx, false)
    }

    /// Key handling for a field WITH an OS IME bridge (`window.handle_input`
    /// registered). Navigation, deletion, and shortcuts are handled here, but
    /// printable text is NOT inserted — it arrives through the platform input
    /// handler (WM_CHAR / IME composition) as `replace_text_in_range`. Inserting
    /// here as well would double every character. Use this for every bridged field.
    pub fn handle_key_ime(
        &mut self,
        event: &KeyDownEvent,
        cx: Option<&mut App>,
    ) -> TextInputAction {
        self.handle_key_inner(event, cx, true)
    }

    fn handle_key_inner(
        &mut self,
        event: &KeyDownEvent,
        cx: Option<&mut App>,
        ime_active: bool,
    ) -> TextInputAction {
        if self.disabled {
            return TextInputAction::Pass;
        }

        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        let command = mods.control || mods.platform;

        // Word-wise movement: Ctrl+Arrow (Windows/Linux) or Option/Alt+Arrow
        // (macOS). Checked before the clipboard branch so Ctrl+Left/Right are
        // not swallowed; letter shortcuts (Ctrl+A/C/X/V) still fall through.
        if (mods.control || mods.alt) && !mods.platform && !mods.function {
            match key {
                "left" | "arrow_left" => {
                    self.move_word_left(mods.shift);
                    return TextInputAction::Consumed;
                }
                "right" | "arrow_right" => {
                    self.move_word_right(mods.shift);
                    return TextInputAction::Consumed;
                }
                _ => {}
            }
        }

        if command && !mods.alt && !mods.function {
            return match key {
                "a" | "A" => {
                    self.select_all();
                    TextInputAction::Consumed
                }
                "c" | "C" => {
                    if self.copy_to_clipboard(cx) {
                        TextInputAction::Consumed
                    } else {
                        TextInputAction::Pass
                    }
                }
                "x" | "X" => {
                    if self.cut_to_clipboard(cx) {
                        TextInputAction::Consumed
                    } else {
                        TextInputAction::Pass
                    }
                }
                "v" | "V" => {
                    if self.paste_from_clipboard(cx) {
                        TextInputAction::Consumed
                    } else {
                        TextInputAction::Pass
                    }
                }
                _ => TextInputAction::Pass,
            };
        }

        match key {
            "backspace" if !self.read_only => {
                self.delete_backward();
                TextInputAction::Consumed
            }
            "delete" if !self.read_only => {
                self.delete_forward();
                TextInputAction::Consumed
            }
            "left" | "arrow_left" => {
                self.move_cursor_left(mods.shift);
                TextInputAction::Consumed
            }
            "right" | "arrow_right" => {
                self.move_cursor_right(mods.shift);
                TextInputAction::Consumed
            }
            "home" => {
                self.move_cursor_to(0, mods.shift);
                TextInputAction::Consumed
            }
            "end" => {
                self.move_cursor_to(self.char_count(), mods.shift);
                TextInputAction::Consumed
            }
            "enter" | "numpad_enter" => TextInputAction::Submit,
            "escape" => TextInputAction::Cancel,
            _ if !self.read_only => {
                if ime_active {
                    // Printable text is delivered by the OS input handler
                    // (WM_CHAR / IME composition) as `replace_text_in_range`, not
                    // from KeyDown — inserting here too would double it. Still
                    // report a character key as consumed so the host treats it as
                    // text rather than dispatching a shortcut.
                    if event.keystroke.key_char.is_some() || printable_text(key).is_some() {
                        TextInputAction::Consumed
                    } else {
                        TextInputAction::Pass
                    }
                } else if let Some(text) = char_to_insert(event) {
                    self.insert_str(text);
                    TextInputAction::Consumed
                } else {
                    TextInputAction::Pass
                }
            }
            _ => TextInputAction::Pass,
        }
    }

    pub fn apply_context_command(&mut self, command: &str, cx: &mut App) -> bool {
        match command {
            TEXT_INPUT_CUT => self.cut_to_clipboard(Some(cx)),
            TEXT_INPUT_COPY => self.copy_to_clipboard(Some(cx)),
            TEXT_INPUT_PASTE => self.paste_from_clipboard(Some(cx)),
            TEXT_INPUT_SELECT_ALL => {
                self.select_all();
                true
            }
            _ => false,
        }
    }

    /// Apply one [`TextInputEvent`] with Unicode- and grapheme-correct semantics.
    ///
    /// This is the centralized, IME-safe edit entry point: every field can drive
    /// editing through this single vocabulary instead of re-deriving cursor math.
    /// Clipboard variants (`Copy`/`Cut`) need an [`App`]; pass it via `cx`. Returns
    /// `true` if the state changed (or the command was actionable).
    pub fn apply_event(&mut self, event: TextInputEvent, cx: Option<&mut App>) -> bool {
        if self.disabled {
            return false;
        }
        match event {
            TextInputEvent::InsertText(text) => {
                if self.read_only {
                    return false;
                }
                self.insert_str(&text);
                true
            }
            TextInputEvent::CompositionStart => true,
            TextInputEvent::CompositionUpdate { text, cursor } => {
                if self.read_only {
                    return false;
                }
                self.set_composition(&text, cursor);
                true
            }
            TextInputEvent::CompositionCommit(text) => {
                if self.read_only {
                    return false;
                }
                self.commit_composition(&text);
                true
            }
            TextInputEvent::CompositionCancel => {
                self.cancel_composition();
                true
            }
            TextInputEvent::DeleteBackward => {
                if self.read_only {
                    return false;
                }
                self.delete_backward();
                true
            }
            TextInputEvent::DeleteForward => {
                if self.read_only {
                    return false;
                }
                self.delete_forward();
                true
            }
            TextInputEvent::MoveLeft => {
                self.move_cursor_left(false);
                true
            }
            TextInputEvent::MoveRight => {
                self.move_cursor_right(false);
                true
            }
            TextInputEvent::MoveWordLeft => {
                self.move_word_left(false);
                true
            }
            TextInputEvent::MoveWordRight => {
                self.move_word_right(false);
                true
            }
            TextInputEvent::MoveToStart => {
                self.move_cursor_to(0, false);
                true
            }
            TextInputEvent::MoveToEnd => {
                self.move_cursor_to(self.char_count(), false);
                true
            }
            TextInputEvent::SelectAll => {
                self.select_all();
                true
            }
            TextInputEvent::ReplaceSelection(text) => {
                if self.read_only {
                    return false;
                }
                self.insert_str(&text);
                true
            }
            TextInputEvent::Copy => self.copy_to_clipboard(cx),
            TextInputEvent::Cut => self.cut_to_clipboard(cx),
            TextInputEvent::Paste(text) => {
                if self.read_only {
                    return false;
                }
                self.insert_str(&text);
                true
            }
        }
    }

    /// Apply a [`TextEditCommand`]. The clipboard variants need an [`App`].
    pub fn apply_command(&mut self, command: TextEditCommand, cx: Option<&mut App>) -> bool {
        match command {
            TextEditCommand::Copy => self.apply_event(TextInputEvent::Copy, cx),
            TextEditCommand::Cut => self.apply_event(TextInputEvent::Cut, cx),
            TextEditCommand::Paste(text) => self.apply_event(TextInputEvent::Paste(text), cx),
            TextEditCommand::SelectAll => self.apply_event(TextInputEvent::SelectAll, cx),
            TextEditCommand::ReplaceSelection(text) => {
                self.apply_event(TextInputEvent::ReplaceSelection(text), cx)
            }
        }
    }

    /// Current selection (anchor + cursor) in committed-`char`-index space.
    pub fn selection(&self) -> TextSelection {
        TextSelection {
            anchor: self.selection_anchor.unwrap_or(self.cursor),
            cursor: self.cursor,
        }
    }

    /// Active IME composition, if the field is currently composing pre-edit text.
    pub fn composition(&self) -> Option<ImeCompositionState> {
        self.marked_range.clone().map(|range| ImeCompositionState {
            text: self.slice_chars(range.clone()),
            range,
            cursor: self.cursor,
        })
    }

    /// Whether an IME composition is currently active.
    pub fn is_composing(&self) -> bool {
        self.marked_range.is_some()
    }

    /// Replace the active composition range (or selection/caret) with pre-edit
    /// `text`, re-mark it, and place the caret `cursor_in_text` graphemes in.
    ///
    /// Mirrors the OS `replace_and_mark_text_in_range` path so the in-app event
    /// API and the platform IME bridge converge on identical behavior.
    fn set_composition(&mut self, text: &str, cursor_in_text: usize) {
        let range = self
            .marked_range
            .clone()
            .or_else(|| self.selection_range())
            .unwrap_or(self.cursor..self.cursor);
        let start = range.start;
        let text_chars = text.chars().count();
        self.replace_char_range(range, text);
        self.marked_range = (text_chars > 0).then_some(start..start + text_chars);
        self.cursor = start + cursor_in_text.min(text_chars);
        self.selection_anchor = None;
    }

    /// Commit the active composition (or selection/caret) to `text`, clearing the
    /// pre-edit mark and leaving the caret after the inserted text.
    fn commit_composition(&mut self, text: &str) {
        let range = self
            .marked_range
            .take()
            .or_else(|| self.selection_range())
            .unwrap_or(self.cursor..self.cursor);
        let start = range.start;
        self.replace_char_range(range, text);
        self.cursor = start + text.chars().count();
        self.selection_anchor = None;
    }

    /// Cancel the active composition, removing the pre-edit text and restoring the
    /// caret to where composition began.
    fn cancel_composition(&mut self) {
        if let Some(range) = self.marked_range.take() {
            self.replace_char_range(range.clone(), "");
            self.cursor = range.start;
            self.selection_anchor = None;
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let range = self.selection_range()?;
        Some(self.slice_chars(range))
    }

    fn copy_to_clipboard(&self, cx: Option<&mut App>) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        let Some(cx) = cx else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn cut_to_clipboard(&mut self, cx: Option<&mut App>) -> bool {
        if self.read_only || self.disabled {
            return false;
        }
        let Some(text) = self.selected_text() else {
            return false;
        };
        let Some(cx) = cx else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.delete_selection();
        true
    }

    fn paste_from_clipboard(&mut self, cx: Option<&mut App>) -> bool {
        if self.read_only || self.disabled {
            return false;
        }
        let Some(cx) = cx else {
            return false;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return false;
        };
        self.insert_str(&text);
        true
    }

    fn char_count(&self) -> usize {
        self.value.chars().count()
    }

    fn char_to_utf16(&self, char_idx: usize) -> usize {
        self.value
            .chars()
            .take(char_idx.min(self.char_count()))
            .map(char::len_utf16)
            .sum()
    }

    fn char_from_utf16(&self, utf16_idx: usize) -> usize {
        let mut utf16_count = 0usize;
        let mut char_count = 0usize;
        for ch in self.value.chars() {
            let next = utf16_count + ch.len_utf16();
            if next > utf16_idx {
                break;
            }
            utf16_count = next;
            char_count += 1;
        }
        char_count
    }

    fn range_to_utf16(&self, range: Range<usize>) -> Range<usize> {
        self.char_to_utf16(range.start)..self.char_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: Range<usize>) -> Range<usize> {
        let start = self.char_from_utf16(range.start);
        let end = self.char_from_utf16(range.end);
        start.min(end)..start.max(end)
    }

    pub fn text_for_utf16_range(
        &self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
    ) -> Option<String> {
        let range = self.range_from_utf16(range_utf16);
        actual_range.replace(self.range_to_utf16(range.clone()));
        Some(self.slice_chars(range))
    }

    pub fn selected_text_range_utf16(&self, ignore_disabled_input: bool) -> Option<UTF16Selection> {
        if self.disabled && !ignore_disabled_input {
            return None;
        }
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        let range = anchor.min(self.cursor)..anchor.max(self.cursor);
        Some(UTF16Selection {
            range: self.range_to_utf16(range),
            reversed: anchor > self.cursor,
        })
    }

    pub fn marked_text_range_utf16(&self) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range.clone()))
    }

    pub fn replace_text_in_utf16_range(&mut self, range_utf16: Option<Range<usize>>, text: &str) {
        if self.disabled || self.read_only {
            return;
        }
        let range = range_utf16
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .or_else(|| self.selection_range())
            .unwrap_or(self.cursor..self.cursor);
        self.replace_char_range(range.clone(), text);
        self.cursor = range.start + text.chars().count();
        self.selection_anchor = None;
        self.marked_range = None;
    }

    pub fn replace_and_mark_text_in_utf16_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_range_utf16: Option<Range<usize>>,
    ) {
        if self.disabled || self.read_only {
            return;
        }
        let range = range_utf16
            .map(|range| self.range_from_utf16(range))
            .or_else(|| self.marked_range.clone())
            .or_else(|| self.selection_range())
            .unwrap_or(self.cursor..self.cursor);
        let start = range.start;
        let text_chars = text.chars().count();
        self.replace_char_range(range, text);
        self.marked_range = (text_chars > 0).then_some(start..start + text_chars);

        if let Some(selected_range_utf16) = selected_range_utf16 {
            let selected = char_range_from_utf16_text(text, selected_range_utf16);
            self.cursor = start + selected.end.min(text_chars);
            self.selection_anchor =
                (selected.start != selected.end).then_some(start + selected.start.min(text_chars));
        } else {
            self.cursor = start + text_chars;
            self.selection_anchor = None;
        }
    }

    pub fn bounds_for_utf16_range(
        &self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(range_utf16);
        let left = bounds.left() + px(TEXT_INPUT_PAD_X + range.start as f32 * TEXT_INPUT_CHAR_W);
        let right = bounds.left() + px(TEXT_INPUT_PAD_X + range.end as f32 * TEXT_INPUT_CHAR_W);
        Some(Bounds::from_corners(
            point(left, bounds.top()),
            point(right.max(left + px(1.0)), bounds.bottom()),
        ))
    }

    fn display_value(&self) -> String {
        if self.is_password {
            "•".repeat(self.char_count())
        } else {
            self.value.clone()
        }
    }

    fn byte_at_char(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(byte, _)| byte)
            .unwrap_or(self.value.len())
    }

    /// Char index of the grapheme-cluster boundary immediately before `char_idx`.
    ///
    /// Moving/deleting by grapheme keeps emoji (incl. ZWJ sequences), Thai
    /// base+vowel/tone stacks, and combining-mark sequences intact instead of
    /// slicing a single user-perceived character in half.
    fn prev_grapheme(&self, char_idx: usize) -> usize {
        if char_idx == 0 {
            return 0;
        }
        let byte = self.byte_at_char(char_idx);
        let prev_byte = self.value[..byte]
            .grapheme_indices(true)
            .next_back()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.value[..prev_byte].chars().count()
    }

    /// Char index of the grapheme-cluster boundary immediately after `char_idx`.
    fn next_grapheme(&self, char_idx: usize) -> usize {
        let byte = self.byte_at_char(char_idx);
        if byte >= self.value.len() {
            return self.char_count();
        }
        let len = self.value[byte..]
            .graphemes(true)
            .next()
            .map(str::len)
            .unwrap_or(0);
        self.value[..byte + len].chars().count()
    }

    /// Char index of the start of the word at/before `char_idx` (Ctrl/Alt+Left).
    ///
    /// Skips any whitespace immediately before the cursor, then lands on the
    /// start of the preceding word using Unicode word boundaries.
    fn prev_word_boundary(&self, char_idx: usize) -> usize {
        let byte = self.byte_at_char(char_idx);
        let head = &self.value[..byte];
        let mut target = 0usize;
        for (idx, piece) in head.split_word_bound_indices() {
            if piece.chars().next().is_some_and(|c| !c.is_whitespace()) {
                target = idx;
            }
        }
        self.value[..target].chars().count()
    }

    /// Char index of the start of the next word after `char_idx` (Ctrl/Alt+Right).
    ///
    /// Consumes the segment under the cursor, then stops at the start of the next
    /// non-whitespace word.
    fn next_word_boundary(&self, char_idx: usize) -> usize {
        let byte = self.byte_at_char(char_idx);
        let tail = &self.value[byte..];
        let mut consumed_first = false;
        for (idx, piece) in tail.split_word_bound_indices() {
            if !consumed_first {
                consumed_first = true;
                continue;
            }
            if piece.chars().next().is_some_and(|c| !c.is_whitespace()) {
                return self.value[..byte + idx].chars().count();
            }
        }
        self.char_count()
    }

    fn selection_range(&self) -> Option<Range<usize>> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    fn slice_chars(&self, range: Range<usize>) -> String {
        self.value
            .chars()
            .skip(range.start)
            .take(range.end - range.start)
            .collect()
    }

    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection_range() else {
            self.clear_selection();
            return false;
        };
        self.unmark_text();
        self.replace_char_range(range.clone(), "");
        self.cursor = range.start;
        self.clear_selection();
        true
    }

    fn replace_char_range(&mut self, range: Range<usize>, text: &str) {
        let start_byte = self.byte_at_char(range.start);
        let end_byte = self.byte_at_char(range.end);
        self.value.replace_range(start_byte..end_byte, text);
    }

    fn insert_str(&mut self, text: &str) {
        self.delete_selection();
        self.unmark_text();
        let byte_pos = self.byte_at_char(self.cursor);
        self.value.insert_str(byte_pos, text);
        self.cursor += text.chars().count();
    }

    fn delete_backward(&mut self) {
        if self.delete_selection() || self.cursor == 0 {
            return;
        }
        self.unmark_text();
        let prev = self.prev_grapheme(self.cursor);
        let start = self.byte_at_char(prev);
        let end = self.byte_at_char(self.cursor);
        self.value.replace_range(start..end, "");
        self.cursor = prev;
    }

    fn delete_forward(&mut self) {
        if self.delete_selection() || self.cursor >= self.char_count() {
            return;
        }
        self.unmark_text();
        let next = self.next_grapheme(self.cursor);
        let start = self.byte_at_char(self.cursor);
        let end = self.byte_at_char(next);
        self.value.replace_range(start..end, "");
    }

    fn move_cursor_to(&mut self, position: usize, extend: bool) {
        let position = position.min(self.char_count());
        if extend {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.unmark_text();
            self.clear_selection();
        }
        self.cursor = position;
        if self.selection_anchor == Some(self.cursor) {
            self.clear_selection();
        }
    }

    fn move_cursor_left(&mut self, extend: bool) {
        if !extend {
            if let Some(range) = self.selection_range() {
                self.cursor = range.start;
                self.clear_selection();
                return;
            }
        }
        let target = self.prev_grapheme(self.cursor);
        self.move_cursor_to(target, extend);
    }

    fn move_cursor_right(&mut self, extend: bool) {
        if !extend {
            if let Some(range) = self.selection_range() {
                self.cursor = range.end;
                self.clear_selection();
                return;
            }
        }
        let target = self.next_grapheme(self.cursor);
        self.move_cursor_to(target, extend);
    }

    fn move_word_left(&mut self, extend: bool) {
        let target = self.prev_word_boundary(self.cursor);
        self.move_cursor_to(target, extend);
    }

    fn move_word_right(&mut self, extend: bool) {
        let target = self.next_word_boundary(self.cursor);
        self.move_cursor_to(target, extend);
    }
}

fn printable_text(key: &str) -> Option<&str> {
    match key {
        "space" => Some(" "),
        k if k.chars().count() == 1 => {
            let c = k.chars().next()?;
            (!c.is_control()).then_some(k)
        }
        _ => None,
    }
}

/// The text a key press should insert into a non-IME field. Prefers the
/// platform-resolved `key_char` (correct for Thai, accents, AltGr, dead-key
/// results, and case), falling back to the logical key for plain ASCII. Control
/// characters (Enter/Tab/Backspace, already handled above) are rejected.
fn char_to_insert(event: &KeyDownEvent) -> Option<&str> {
    if let Some(key_char) = event.keystroke.key_char.as_deref() {
        if !key_char.is_empty() && !key_char.chars().next().is_some_and(char::is_control) {
            return Some(key_char);
        }
    }
    printable_text(event.keystroke.key.as_str())
}

fn char_range_from_utf16_text(text: &str, range: Range<usize>) -> Range<usize> {
    fn char_from_utf16(text: &str, utf16_idx: usize) -> usize {
        let mut utf16_count = 0usize;
        let mut char_count = 0usize;
        for ch in text.chars() {
            let next = utf16_count + ch.len_utf16();
            if next > utf16_idx {
                break;
            }
            utf16_count = next;
            char_count += 1;
        }
        char_count
    }

    let start = char_from_utf16(text, range.start);
    let end = char_from_utf16(text, range.end);
    start.min(end)..start.max(end)
}

pub fn text_input_context_entries(
    state: &TextInputState,
    clipboard_has_text: bool,
) -> Vec<ContextMenuEntry> {
    vec![
        menu_item("Cut", TEXT_INPUT_CUT, state.can_cut(), Some("Ctrl+X")),
        menu_item("Copy", TEXT_INPUT_COPY, state.can_copy(), Some("Ctrl+C")),
        menu_item(
            "Paste",
            TEXT_INPUT_PASTE,
            state.can_paste() && clipboard_has_text,
            Some("Ctrl+V"),
        ),
        ContextMenuEntry::Separator,
        menu_item(
            "Select All",
            TEXT_INPUT_SELECT_ALL,
            state.can_select_all(),
            Some("Ctrl+A"),
        ),
    ]
}

/// Convenience helper: binds mouse drag selection to a `TextInputState` stored
/// inside an entity, without duplicating handler boilerplate.
pub fn bind_mouse_selection<T: gpui::Render>(
    target: gpui::Entity<T>,
    get: impl Fn(&mut T) -> &mut TextInputState + Send + Sync + 'static,
) -> TextInputCallbacks {
    // Wire the context menu here rather than at each call site. Every field
    // built through this helper already hands over the entity and a mutable
    // accessor, which is everything the menu needs — leaving it to callers is
    // why most text fields in the app shipped without Cut/Copy/Paste at all.
    let get = Arc::new(get);
    let open_target = target.clone();
    let open_get = get.clone();
    let command_target = target.clone();
    let command_get = get.clone();

    TextInputCallbacks {
        on_context_menu: Some(Arc::new(move |pos: &(f32, f32), window, cx| {
            let (x, y) = *pos;
            let viewport = window.viewport_size();
            let clipboard_has_text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty());
            let _ = open_target.update(cx, |this, cx| {
                open_get(this).context_menu = Some(TextContextMenuAnchor {
                    x,
                    y,
                    viewport_width: viewport.width.into(),
                    viewport_height: viewport.height.into(),
                    clipboard_has_text,
                });
                cx.notify();
            });
        })),
        on_context_command: Some(Arc::new(move |command: Option<&str>, _window, cx| {
            let command = command.map(|c| c.to_string());
            let _ = command_target.update(cx, |this, cx| {
                if let Some(command) = command {
                    let _ = command_get(this).apply_context_command(&command, cx);
                }
                command_get(this).context_menu = None;
                cx.notify();
            });
        })),
        on_mouse: Some(Arc::new(move |event: &TextInputMouseEvent, _w, cx| {
            let phase = event.phase;
            let index = event.index;
            let extend = event.extend;
            let _ = target.update(cx, |this, cx| {
                let input = get(this);
                match phase {
                    TextInputMousePhase::Down => input.handle_mouse_down(index, extend),
                    TextInputMousePhase::Drag => input.handle_mouse_drag(index),
                    TextInputMousePhase::Up => input.handle_mouse_up(),
                }
                cx.notify();
            });
        })),
    }
}

fn menu_item(
    label: &'static str,
    command: &'static str,
    enabled: bool,
    shortcut: Option<&'static str>,
) -> ContextMenuEntry {
    let item = if enabled {
        ContextMenuEntry::item(label, command)
    } else {
        ContextMenuEntry::disabled_item(label, command)
    };
    if let Some(shortcut) = shortcut {
        item.with_shortcut(crate::keymap::accel_display(shortcut))
    } else {
        item
    }
}

pub fn text_field(state: &TextInputState, focused: bool) -> impl IntoElement {
    text_field_with_callbacks(state, focused, TextInputCallbacks::default())
}

pub fn text_field_with_callbacks(
    state: &TextInputState,
    focused: bool,
    callbacks: TextInputCallbacks,
) -> impl IntoElement {
    text_field_inner(state, focused, callbacks, None)
}

pub fn text_field_with_callbacks_and_ime<V: EntityInputHandler>(
    state: &TextInputState,
    focused: bool,
    callbacks: TextInputCallbacks,
    ime_target: Entity<V>,
) -> impl IntoElement {
    text_field_inner(
        state,
        focused,
        callbacks,
        Some(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .right_0()
                .child(TextInputImeLayer {
                    target: ime_target,
                    focus_handle: state.focus_handle.clone(),
                })
                .into_any_element(),
        ),
    )
}

/// Standalone OS-IME bridge for a *multi-field* window whose inputs are rendered
/// behind panel functions (so the per-field [`text_field_with_callbacks_and_ime`]
/// can't reach `cx.entity()`).
///
/// Drop this into the window root as a child whenever one of its fields owns
/// focus, passing the window entity (which must `impl EntityInputHandler` and
/// route every call to its currently-focused field) and that field's
/// [`FocusHandle`]. It registers the platform input handler against the focused
/// handle so CJK/Thai composition, dead keys, and pasted Unicode reach the field.
/// Non-interactive and absolutely positioned, so it never blocks clicks or
/// affects layout. Coexists with the raw `handle_key_with_clipboard` path — GPUI
/// suppresses key dispatch for keystrokes the IME consumes, so text never doubles.
pub fn ime_input_bridge<V: EntityInputHandler>(
    ime_target: Entity<V>,
    focus_handle: FocusHandle,
) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .child(TextInputImeLayer {
            target: ime_target,
            focus_handle,
        })
}

fn text_field_inner(
    state: &TextInputState,
    focused: bool,
    callbacks: TextInputCallbacks,
    ime_layer: Option<gpui::AnyElement>,
) -> impl IntoElement {
    let fh_click = state.focus_handle.clone();
    let fh_right = state.focus_handle.clone();
    let disabled = state.disabled;
    let fh_track = state.focus_handle.clone().tab_stop(!disabled);
    let fh_out = state.focus_handle.clone();
    let focused = focused && !disabled;
    let blur_outside = state.blur_on_click_outside;
    let accessible_label = state.accessible_label();
    let accessible_value = state.value.clone();
    let read_only = state.read_only;
    let is_password = state.is_password;
    let value = state.display_value();
    let placeholder = state.placeholder.clone().unwrap_or_default();
    let selection = state.selection_range();
    let cursor = state.cursor.min(value.chars().count());
    let on_context_menu = callbacks.on_context_menu.clone();
    // Menu drawn by the field itself, for owners that use `bind_mouse_selection`
    // and do not render one. `deferred` lifts it above the field's siblings, and
    // the anchor carries the viewport it was measured against so the panel is
    // still clamped into the window it belongs to.
    let context_overlay = state
        .context_menu
        .zip(callbacks.on_context_command.clone())
        .map(|(anchor, on_command)| {
            let on_close = on_command.clone();
            gpui::deferred(crate::components::context_menu::context_menu_overlay(
                text_input_context_entries(state, anchor.clipboard_has_text),
                anchor.x,
                anchor.y,
                anchor.viewport_width,
                anchor.viewport_height,
                Arc::new(move |command: &String, window, cx| {
                    on_command(Some(command.as_str()), window, cx);
                }),
                Arc::new(move |_: &(), window, cx| {
                    on_close(None, window, cx);
                }),
            ))
        });
    let on_mouse_down = callbacks.on_mouse.clone();
    let on_mouse_move = callbacks.on_mouse.clone();
    let on_mouse_up = callbacks.on_mouse.clone();
    let on_mouse_up_out = callbacks.on_mouse.clone();
    // Per-render shaped-line layout, written by the hit probe in prepaint and read
    // by the mouse handlers below to map a pointer x to a grapheme index.
    let hit: FieldHitCell = Rc::new(RefCell::new(None));
    let hit_down = hit.clone();
    let hit_move = hit.clone();

    let border = if focused {
        Colors::border_focus()
    } else {
        Colors::border_subtle()
    };
    let bg = if focused {
        Colors::surface_card()
    } else {
        Colors::surface_input()
    };

    let content: gpui::AnyElement = if value.is_empty() {
        div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .when(focused, |d| d.child(caret()))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.0))
                    .text_color(Colors::text_faint())
                    .child(placeholder),
            )
            .into_any_element()
    } else if let Some(range) = selection {
        let before: String = value.chars().take(range.start).collect();
        let selected: String = value
            .chars()
            .skip(range.start)
            .take(range.end - range.start)
            .collect();
        let after: String = value.chars().skip(range.end).collect();
        div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .overflow_hidden()
            .child(text_segment(before, false))
            .when(cursor == range.start && focused, |d| d.child(caret()))
            .child(text_segment(selected, true))
            .when(cursor == range.end && focused, |d| d.child(caret()))
            .child(text_segment(after, false))
            .into_any_element()
    } else if let Some(range) = state.marked_range.clone() {
        let before: String = value.chars().take(range.start).collect();
        let marked: String = value
            .chars()
            .skip(range.start)
            .take(range.end - range.start)
            .collect();
        let after: String = value.chars().skip(range.end).collect();
        div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .overflow_hidden()
            .child(text_segment(before, false))
            .child(text_segment(marked, true))
            .when(focused, |d| d.child(caret()))
            .child(text_segment(after, false))
            .into_any_element()
    } else {
        let before: String = value.chars().take(cursor).collect();
        let after: String = value.chars().skip(cursor).collect();
        div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .overflow_hidden()
            .child(text_segment(before, false))
            .when(focused, |d| d.child(caret()))
            .child(text_segment(after, false))
            .into_any_element()
    };

    div()
        .id(state.element_id)
        .role(Role::TextInput)
        .aria_label(accessible_label)
        .aria_disabled(disabled)
        .aria_read_only(read_only)
        .when(!is_password, |this| this.aria_value(accessible_value))
        .relative()
        .track_focus(&fh_track)
        .focus_visible(|style| style.border_color(Colors::border_focus()))
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(30.0))
        .px(px(9.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(border)
        .bg(bg)
        .opacity(if disabled { 0.48 } else { 1.0 })
        .cursor(if disabled {
            gpui::CursorStyle::Arrow
        } else {
            gpui::CursorStyle::IBeam
        })
        .when(focused, |this| {
            this.shadow(vec![gpui::BoxShadow {
                color: Colors::with_alpha(Colors::border_focus(), 0.15).into(),
                offset: gpui::point(px(0.0), px(0.0)),
                blur_radius: px(0.0),
                spread_radius: px(1.0),
                inset: false,
            }])
        })
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            if !disabled {
                fh_click.focus(window, cx);
                cx.stop_propagation();
                if let Some(cb) = on_mouse_down.as_ref() {
                    let gx: f32 = event.position.x.into();
                    cb(
                        &TextInputMouseEvent {
                            phase: TextInputMousePhase::Down,
                            index: hit_index(&hit_down, gx),
                            extend: event.modifiers.shift,
                        },
                        window,
                        cx,
                    );
                }
            }
        })
        .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
            if disabled {
                return;
            }
            if let Some(cb) = on_mouse_move.as_ref() {
                let gx: f32 = event.position.x.into();
                cb(
                    &TextInputMouseEvent {
                        phase: TextInputMousePhase::Drag,
                        index: hit_index(&hit_move, gx),
                        extend: false,
                    },
                    window,
                    cx,
                );
            }
        })
        .on_mouse_up(
            MouseButton::Left,
            move |_event: &MouseUpEvent, window, cx| {
                if disabled {
                    return;
                }
                if let Some(cb) = on_mouse_up.as_ref() {
                    cb(
                        &TextInputMouseEvent {
                            phase: TextInputMousePhase::Up,
                            index: 0,
                            extend: false,
                        },
                        window,
                        cx,
                    );
                }
            },
        )
        .on_mouse_up_out(
            MouseButton::Left,
            move |_event: &MouseUpEvent, window, cx| {
                if disabled {
                    return;
                }
                if let Some(cb) = on_mouse_up_out.as_ref() {
                    cb(
                        &TextInputMouseEvent {
                            phase: TextInputMousePhase::Up,
                            index: 0,
                            extend: false,
                        },
                        window,
                        cx,
                    );
                }
            },
        )
        // Release focus when the user mouses down outside this field (capture
        // phase, so a click landing on another field still transfers focus to it
        // afterward). Only when the field opted in, and only if it is the focused
        // one — so dialogs that keep their field focused are unaffected.
        .when(blur_outside, |this| {
            this.on_mouse_down_out(move |_event, window, _cx| {
                if fh_out.is_focused(window) {
                    window.blur();
                }
            })
        })
        .on_mouse_down(MouseButton::Right, move |event, window, cx| {
            if disabled {
                return;
            }
            fh_right.focus(window, cx);
            if let Some(callback) = on_context_menu.as_ref() {
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                callback(&(x, y), window, cx);
            }
        })
        .child(
            // Wrapper owns the text content rect; the transparent hit probe shapes
            // the same text to drive grapheme-accurate pointer hit-testing without
            // affecting the visible layout.
            div()
                .relative()
                .flex()
                .flex_row()
                .items_center()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(content)
                .child(div().absolute().inset_0().child(TextHitProbe {
                    text: value.clone(),
                    hit: hit.clone(),
                })),
        )
        .children(ime_layer)
        .children(context_overlay)
}

fn text_segment(text: String, selected: bool) -> impl IntoElement {
    div()
        .min_w(px(0.0))
        .overflow_hidden()
        .truncate()
        .rounded(px(crate::theme::radius::CONTROL))
        .when(selected, |d| {
            d.bg(Colors::with_alpha(Colors::accent_primary(), 0.25))
                .px(px(2.0))
        })
        .text_size(px(12.0))
        .text_color(Colors::text_primary())
        .child(text)
}

fn caret() -> impl IntoElement {
    div()
        .flex_none()
        .w(px(1.5))
        .h(px(15.0))
        .bg(Colors::accent_primary())
        .rounded(px(crate::theme::radius::CONTROL))
}

/// Whether a key should keep firing on OS auto-repeat while a text field is
/// focused. Editing/navigation keys repeat (hold-to-delete, hold-to-move);
/// submit/cancel/shortcut keys do not. The per-context key handlers use this to
/// stop swallowing repeated KeyDown events for these keys.
pub fn is_repeatable_edit_key(event: &KeyDownEvent) -> bool {
    matches!(
        event.keystroke.key.as_str(),
        "backspace"
            | "delete"
            | "left"
            | "arrow_left"
            | "right"
            | "arrow_right"
            | "up"
            | "arrow_up"
            | "down"
            | "arrow_down"
            | "home"
            | "end"
    )
}

/// Shaped-line layout for one text field, recorded by [`TextHitProbe`] during
/// prepaint and read by the field's mouse handlers to map a pointer x to a
/// grapheme index. Behind `Rc<RefCell>` because the probe (writer) and the mouse
/// closures (readers) are built in the same render pass.
struct FieldHit {
    bounds: Bounds<Pixels>,
    line: ShapedLine,
    text: String,
}

type FieldHitCell = Rc<RefCell<Option<FieldHit>>>;

/// Map a window-space pointer x to a grapheme/char index using the last shaped
/// line. Returns 0 before the first paint. The byte offset from the shaped line
/// is always a glyph boundary, so the char conversion is grapheme-safe.
fn hit_index(hit: &FieldHitCell, global_x: f32) -> usize {
    let guard = hit.borrow();
    let Some(h) = guard.as_ref() else {
        return 0;
    };
    let left: f32 = h.bounds.left().into();
    let local = (global_x - left).max(0.0);
    let byte = h.line.closest_index_for_x(px(local)).min(h.text.len());
    h.text[..byte].chars().count()
}

/// Invisible overlay element that shapes the field's text each frame and records
/// the layout for grapheme-accurate mouse hit-testing. Paints nothing and adds no
/// hitbox, so it never affects rendering or click routing.
struct TextHitProbe {
    text: String,
    hit: FieldHitCell,
}

impl IntoElement for TextHitProbe {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextHitProbe {
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
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // Shape with the ambient UI font at the field's fixed 12px text size — the
        // same as the visible `text_segment` glyphs — so x positions line up.
        let (font, color) = {
            let style = window.text_style();
            (style.font(), style.color)
        };
        let runs = if self.text.is_empty() {
            Vec::new()
        } else {
            vec![TextRun {
                len: self.text.len(),
                font,
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }]
        };
        let line = window
            .text_system()
            .shape_line(self.text.clone().into(), px(12.0), &runs, None);
        *self.hit.borrow_mut() = Some(FieldHit {
            bounds,
            line,
            text: self.text.clone(),
        });
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
}

struct TextInputImeLayer<V: EntityInputHandler> {
    target: Entity<V>,
    focus_handle: FocusHandle,
}

impl<V: EntityInputHandler> IntoElement for TextInputImeLayer<V> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<V: EntityInputHandler> Element for TextInputImeLayer<V> {
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
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.handle_input(
            &self.focus_handle,
            ElementInputHandler::new(bounds, self.target.clone()),
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    //! Unicode- and IME-correctness tests for the shared editing core.
    //!
    //! These drive [`TextEditBuffer`] directly (no GPUI focus handle needed),
    //! exercising the exact event vocabulary every migrated field speaks. The
    //! grapheme/composition behavior proven here holds for every input in the app,
    //! because they all share this one buffer.
    use super::{TextEditBuffer, TextInputEvent as Ev};

    fn buf(s: &str) -> TextEditBuffer {
        let mut b = TextEditBuffer::default();
        b.set_value(s); // caret lands at end
        b
    }

    // Convenience: apply an event with no clipboard/App context.
    fn ev(b: &mut TextEditBuffer, event: Ev) -> bool {
        b.apply_event(event, None)
    }

    #[test]
    fn ascii_insert() {
        let mut b = TextEditBuffer::default();
        ev(&mut b, Ev::InsertText("hello".into()));
        assert_eq!(b.value, "hello");
        assert_eq!(b.cursor, 5);
    }

    #[test]
    fn thai_insert_and_backspace() {
        // "กิน" = ก (base) + ิ (combining sara-i) + น. Graphemes: ["กิ", "น"].
        let mut b = TextEditBuffer::default();
        ev(&mut b, Ev::InsertText("กิน".into()));
        assert_eq!(b.value, "กิน");
        ev(&mut b, Ev::DeleteBackward);
        assert_eq!(b.value, "กิ", "backspace removes the trailing น only");
        ev(&mut b, Ev::DeleteBackward);
        assert_eq!(
            b.value, "",
            "backspace removes the base+vowel cluster as one unit"
        );
    }

    #[test]
    fn japanese_composition_update_then_commit() {
        let mut b = buf("a");
        ev(&mut b, Ev::CompositionStart);
        ev(
            &mut b,
            Ev::CompositionUpdate {
                text: "か".into(),
                cursor: 1,
            },
        );
        assert_eq!(b.value, "aか");
        assert!(b.is_composing());
        assert_eq!(b.composition().unwrap().text, "か");
        ev(
            &mut b,
            Ev::CompositionUpdate {
                text: "かな".into(),
                cursor: 2,
            },
        );
        assert_eq!(b.value, "aかな");
        ev(&mut b, Ev::CompositionCommit("仮名".into()));
        assert_eq!(b.value, "a仮名");
        assert!(!b.is_composing());
        assert_eq!(b.cursor, 3);
    }

    #[test]
    fn japanese_composition_cancel_restores_value() {
        let mut b = buf("ab");
        b.cursor = 1; // between a and b
        ev(
            &mut b,
            Ev::CompositionUpdate {
                text: "き".into(),
                cursor: 1,
            },
        );
        assert_eq!(b.value, "aきb");
        ev(&mut b, Ev::CompositionCancel);
        assert_eq!(b.value, "ab", "cancel removes the pre-edit text");
        assert_eq!(b.cursor, 1, "caret returns to where composition began");
        assert!(!b.is_composing());
    }

    #[test]
    fn chinese_and_korean_composition_commit() {
        let mut zh = TextEditBuffer::default();
        ev(
            &mut zh,
            Ev::CompositionUpdate {
                text: "ni".into(),
                cursor: 2,
            },
        );
        ev(&mut zh, Ev::CompositionCommit("你".into()));
        assert_eq!(zh.value, "你");
        assert!(!zh.is_composing());

        let mut ko = TextEditBuffer::default();
        ev(
            &mut ko,
            Ev::CompositionUpdate {
                text: "ㅎ".into(),
                cursor: 1,
            },
        );
        ev(
            &mut ko,
            Ev::CompositionUpdate {
                text: "한".into(),
                cursor: 1,
            },
        );
        ev(&mut ko, Ev::CompositionCommit("한".into()));
        assert_eq!(ko.value, "한");
    }

    #[test]
    fn emoji_insert() {
        let mut b = TextEditBuffer::default();
        ev(&mut b, Ev::InsertText("👍".into()));
        assert_eq!(b.value, "👍");
        assert_eq!(b.cursor, 1, "thumbs-up is a single scalar");
    }

    #[test]
    fn emoji_backspace_removes_whole_zwj_cluster() {
        // Family emoji: 👨‍👩‍👧 = 5 scalars joined by ZWJ, one grapheme cluster.
        let family = "👨‍👩‍👧";
        let mut b = buf(family);
        ev(&mut b, Ev::DeleteBackward);
        assert_eq!(b.value, "", "the entire ZWJ cluster is deleted as one unit");

        let mut b2 = buf("a👍");
        ev(&mut b2, Ev::DeleteBackward);
        assert_eq!(b2.value, "a");
    }

    #[test]
    fn combining_marks_delete_as_one() {
        let e_acute = "e\u{0301}"; // e + combining acute accent (one grapheme)
        let mut b = buf(e_acute);
        ev(&mut b, Ev::DeleteBackward);
        assert_eq!(b.value, "", "base + combining mark deleted together");
    }

    #[test]
    fn selection_replace_with_unicode() {
        let mut b = buf("ก👍ข");
        ev(&mut b, Ev::SelectAll);
        ev(&mut b, Ev::ReplaceSelection("世界".into()));
        assert_eq!(b.value, "世界");
        assert_eq!(b.cursor, 2);
    }

    #[test]
    fn paste_unicode_text() {
        let mut b = buf("a");
        ev(&mut b, Ev::Paste("日本語".into()));
        assert_eq!(b.value, "a日本語");
        assert_eq!(b.cursor, 4);
    }

    #[test]
    fn select_all_then_replace() {
        let mut b = buf("hello");
        ev(&mut b, Ev::SelectAll);
        ev(&mut b, Ev::InsertText("X".into()));
        assert_eq!(b.value, "X");
    }

    #[test]
    fn delete_forward_with_unicode() {
        let mut b = buf("👍a");
        b.cursor = 0;
        ev(&mut b, Ev::DeleteForward);
        assert_eq!(
            b.value, "a",
            "forward-delete removes the leading emoji grapheme"
        );

        let mut b2 = buf("e\u{0301}x");
        b2.cursor = 0;
        ev(&mut b2, Ev::DeleteForward);
        assert_eq!(
            b2.value, "x",
            "forward-delete removes base + combining together"
        );
    }

    #[test]
    fn cursor_movement_across_grapheme_clusters() {
        // "a" + family(5 scalars) + "b" = 7 chars, 3 graphemes.
        let mut b = buf("a👨‍👩‍👧b");
        b.cursor = 0;
        ev(&mut b, Ev::MoveRight);
        assert_eq!(b.cursor, 1, "past 'a'");
        ev(&mut b, Ev::MoveRight);
        assert_eq!(b.cursor, 6, "jumps the whole 5-scalar family cluster");
        ev(&mut b, Ev::MoveRight);
        assert_eq!(b.cursor, 7, "past 'b'");
        ev(&mut b, Ev::MoveLeft);
        ev(&mut b, Ev::MoveLeft);
        assert_eq!(b.cursor, 1, "left jumps back over the whole cluster");
    }

    #[test]
    fn composition_commit_over_selected_text() {
        let mut b = buf("hello");
        ev(&mut b, Ev::SelectAll);
        ev(
            &mut b,
            Ev::CompositionUpdate {
                text: "あ".into(),
                cursor: 1,
            },
        );
        assert_eq!(b.value, "あ", "composition replaces the selection");
        ev(&mut b, Ev::CompositionCommit("亜".into()));
        assert_eq!(b.value, "亜");
    }

    #[test]
    fn word_movement_skips_whole_words() {
        let mut b = buf("foo bar baz");
        ev(&mut b, Ev::MoveToStart);
        ev(&mut b, Ev::MoveWordRight);
        assert_eq!(b.cursor, 4, "to start of 'bar'");
        ev(&mut b, Ev::MoveWordRight);
        assert_eq!(b.cursor, 8, "to start of 'baz'");
        ev(&mut b, Ev::MoveWordLeft);
        assert_eq!(b.cursor, 4, "back to start of 'bar'");
    }

    #[test]
    fn read_only_blocks_edits_but_not_movement() {
        let mut b = buf("hi");
        b.read_only = true;
        assert!(!ev(&mut b, Ev::InsertText("x".into())));
        assert_eq!(b.value, "hi");
        assert!(!ev(&mut b, Ev::DeleteBackward));
        assert_eq!(b.value, "hi");
        // movement still allowed
        assert!(ev(&mut b, Ev::MoveToStart));
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn disabled_rejects_all_events() {
        let mut b = buf("hi");
        b.disabled = true;
        assert!(!ev(&mut b, Ev::InsertText("x".into())));
        assert!(!ev(&mut b, Ev::MoveLeft));
        assert_eq!(b.value, "hi");
    }

    // ── Mouse drag selection (Bug 1) ────────────────────────────────────────
    // These drive the same buffer primitives the field's mouse handlers call,
    // with the pointer→index hit-test (shaped line) stubbed by an explicit index.

    #[test]
    fn mouse_drag_selection_left_to_right() {
        let mut b = buf("hello world");
        b.place_cursor(2); // mouse down
        assert!(b.selection().is_empty());
        b.select_to(7); // drag right
        assert_eq!(b.selection().range(), 2..7);
        assert_eq!(b.cursor, 7);
    }

    #[test]
    fn mouse_drag_selection_right_to_left() {
        let mut b = buf("hello world");
        b.place_cursor(8); // mouse down
        b.select_to(3); // drag left
        let sel = b.selection();
        assert_eq!(sel.range(), 3..8);
        assert_eq!(sel.anchor, 8, "anchor stays where the drag began");
        assert_eq!(b.cursor, 3);
    }

    #[test]
    fn click_release_collapses_empty_selection() {
        let mut b = buf("hello");
        b.place_cursor(3);
        b.collapse_empty_selection(); // mouse up after a plain click
        assert!(!b.has_selection());
        assert_eq!(b.cursor, 3);
    }

    #[test]
    fn drag_release_keeps_nonempty_selection() {
        let mut b = buf("hello world");
        b.place_cursor(2);
        b.select_to(7);
        b.collapse_empty_selection(); // non-empty -> preserved
        assert_eq!(b.selection().range(), 2..7);
        assert!(b.has_selection());
    }

    #[test]
    fn drag_selection_is_grapheme_safe() {
        // chars: a(0) 👍(1) ก(2) ; selecting 1..3 grabs the emoji + Thai char.
        let mut b = buf("a👍ก");
        b.place_cursor(1);
        b.select_to(3);
        assert_eq!(b.selected_text().as_deref(), Some("👍ก"));
    }

    #[test]
    fn mouse_click_preserves_active_composition_until_commit() {
        let mut b = buf("ab");
        b.cursor = 1;
        ev(
            &mut b,
            Ev::CompositionUpdate {
                text: "き".into(),
                cursor: 1,
            },
        );
        assert_eq!(b.value, "aきb");

        b.place_cursor(0);
        assert!(b.is_composing());
        ev(&mut b, Ev::CompositionCommit("亜".into()));

        assert_eq!(
            b.value, "a亜b",
            "commit must replace the old pre-edit range"
        );
    }

    #[test]
    fn selection_backspace_then_repeat_deletes_before_cursor() {
        // Bug 3 + selection: first Backspace removes the selection, subsequent
        // (auto-repeat) Backspaces delete char-by-char before the caret.
        let mut b = buf("abcdef");
        b.place_cursor(1);
        b.select_to(4); // select "bcd"
        ev(&mut b, Ev::DeleteBackward);
        assert_eq!(b.value, "aef");
        ev(&mut b, Ev::DeleteBackward);
        assert_eq!(b.value, "ef");
    }

    // ── Key repeat classification (Bug 3) ───────────────────────────────────

    fn key_event(key: &str) -> gpui::KeyDownEvent {
        gpui::KeyDownEvent {
            keystroke: gpui::Keystroke {
                key: key.to_string(),
                ..Default::default()
            },
            is_held: true,
            prefer_character_input: false,
        }
    }

    #[test]
    fn repeatable_edit_keys_are_classified() {
        use super::is_repeatable_edit_key;
        for k in [
            "backspace",
            "delete",
            "left",
            "arrow_left",
            "right",
            "home",
            "end",
        ] {
            assert!(is_repeatable_edit_key(&key_event(k)), "{k} should repeat");
        }
        for k in ["enter", "escape", "a", "tab"] {
            assert!(
                !is_repeatable_edit_key(&key_event(k)),
                "{k} should not repeat"
            );
        }
    }
}

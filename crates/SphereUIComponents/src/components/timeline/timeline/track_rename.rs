//! Inline track rename in the track header: the session the arrangement owns
//! while a name is edited in place, how keys and app commands reach it, and the
//! IME bridge that feeds it composed text.
//!
//! A plain double-click on a header's name opens a dense field with the name
//! selected. Enter commits, Escape cancels, and a press anywhere else in the
//! window — or any other way the field loses focus while the window is active —
//! commits. Deactivating the window (Cmd+Tab, a plug-in editor window) leaves
//! the edit open. A commit is one `SetTrackName` undo step; a blank or unchanged
//! name records nothing.

use super::*;
use crate::components::text_input::{
    TextContextMenuAnchor, TextInputAction, TextInputCallbacks, TextInputEvent,
    TextInputMouseEvent, TextInputMousePhase, TextInputState, TextSelection, TEXT_INPUT_COPY,
    TEXT_INPUT_CUT, TEXT_INPUT_PASTE, TEXT_INPUT_SELECT_ALL,
};
use crate::components::timeline::timeline_state::{is_arrangement_hidden_track, TrackRowLayout};
use crate::components::timeline::track_header::{TrackHeaderRename, TRACK_NAME_FIELD_HEIGHT};
use gpui::{Bounds, Entity, EntityInputHandler, FocusHandle, KeyDownEvent, Pixels, UTF16Selection};
use std::ops::Range;
use std::sync::Arc;

/// An open inline rename of one track's name.
///
/// The arrangement owns it because the arrangement draws the header. The field
/// has a focus handle of its own for the length of the session, and every way
/// the session ends hands focus back to the studio's anchor, so no key is ever
/// routed to a field that is no longer drawn.
pub(crate) struct TrackRenameSession {
    track_id: String,
    input: TextInputState,
    /// The window the field lives in, for an end that arrives without one (a
    /// menu command) and still has to hand focus back.
    window_handle: gpui::AnyWindowHandle,
    /// Commits when the field loses focus inside the active window.
    _blur: Subscription,
}

/// How [`Timeline::handle_track_rename_key`] settled a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackRenameKeyOutcome {
    /// The field used the key, or swallowed it so no shortcut sees it.
    Consumed,
    /// Enter or Escape ended the session.
    Finished,
    /// A Cmd/Ctrl/Alt chord the field has no use for. The command bound to it
    /// goes through [`track_rename_command_policy`] with this keystroke's
    /// chord.
    PassCommand,
}

/// What one key does to an open rename, from the field's verdict and the
/// modifiers held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackRenameKeyStep {
    Commit,
    Cancel,
    Consume,
    PassToCommands,
}

fn track_rename_key_step(
    action: TextInputAction,
    modifiers: &gpui::Modifiers,
) -> TrackRenameKeyStep {
    match action {
        TextInputAction::Submit => TrackRenameKeyStep::Commit,
        TextInputAction::Cancel => TrackRenameKeyStep::Cancel,
        TextInputAction::Consumed => TrackRenameKeyStep::Consume,
        // A chord may be a command worth running (save, undo); the policy
        // decides. A plain key the field ignores — an arrow up or down, a
        // function key — must still not reach the arrangement's shortcuts.
        TextInputAction::Pass
            if modifiers.control || modifiers.platform || modifiers.alt || modifiers.function =>
        {
            TrackRenameKeyStep::PassToCommands
        }
        TextInputAction::Pass => TrackRenameKeyStep::Consume,
    }
}

/// A text edit the open rename field performs for an app command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackRenameEdit {
    WordLeft,
    WordRight,
    SelectAll,
    Cut,
    Copy,
    Paste,
}

/// What an app command does while a track rename is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackRenameCommandPolicy {
    /// Commit the name, then run the command: a save keeps the new name, and
    /// undo takes the whole rename back as one step.
    CommitThenRun,
    /// The field performs it. On macOS a menu key equivalent reaches the app
    /// before any key listener, so Option+Left arrives as `transport:rewind`,
    /// and Cmd+A as a select-all bound to some other surface.
    Edit(TrackRenameEdit),
    /// Nothing happens until the rename ends: the chord is one a text field
    /// types with (Option+K types `˚` on macOS) or a bare key.
    Swallow,
}

/// A chord, as a name being typed sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackRenameChord {
    /// Cmd or Ctrl is held: a shortcut, never text.
    Command,
    /// Option+Left: a word move in a text field.
    WordLeft,
    /// Option+Right: a word move in a text field.
    WordRight,
    /// Anything else: Option without Cmd/Ctrl, which types a character on
    /// macOS, or a bare, Shift or Fn key.
    Typing,
}

impl TrackRenameChord {
    /// Classify a canonical accelerator token, `alt+ctrl+shift+key` in that
    /// order, where Cmd and Ctrl both read `ctrl` (see
    /// [`crate::keymap::canonical_accel`]).
    fn of_token(token: &str) -> Self {
        let (alt, rest) = match token.strip_prefix("alt+") {
            Some(rest) => (true, rest),
            None => (false, token),
        };
        if rest.starts_with("ctrl+") {
            return Self::Command;
        }
        match (alt, rest) {
            (true, "left") => Self::WordLeft,
            (true, "right") => Self::WordRight,
            _ => Self::Typing,
        }
    }

    /// An authored accelerator such as `"Ctrl+Shift+T"` or `"Alt+K"`; `None`
    /// when it names no key.
    pub(crate) fn of_accelerator(accelerator: &str) -> Option<Self> {
        crate::keymap::canonical_accel(accelerator).map(|token| Self::of_token(&token))
    }

    /// The keystroke that was pressed.
    pub(crate) fn of_keystroke(keystroke: &gpui::Keystroke) -> Self {
        crate::keymap::canonical_keystroke(keystroke)
            .map_or(Self::Typing, |token| Self::of_token(&token))
    }

    /// The chord a command that arrived without its keystroke most likely
    /// came from, given every accelerator bound to it. The macOS menubar runs
    /// a click and a key equivalent through the same action, so this is all
    /// there is to go on: any Cmd/Ctrl accelerator makes it a shortcut, and
    /// with none at all (`None`) it can only have been a click.
    pub(crate) fn likeliest<'a>(accelerators: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut likeliest: Option<Self> = None;
        for chord in accelerators.into_iter().filter_map(Self::of_accelerator) {
            likeliest = Some(match (likeliest, chord) {
                (_, Self::Command) | (Some(Self::Command), _) => Self::Command,
                (Some(word @ (Self::WordLeft | Self::WordRight)), _) => word,
                (_, chord) => chord,
            });
        }
        likeliest
    }
}

/// Decide what the (normalised) `command_id` does while a track rename is
/// open. `chord` is the keystroke that ran it, or for a command that arrived
/// without one the [`TrackRenameChord::likeliest`] of its accelerators; `None`
/// is a click. See [`TrackRenameCommandPolicy`].
pub(crate) fn track_rename_command_policy(
    command_id: &str,
    chord: Option<TrackRenameChord>,
) -> TrackRenameCommandPolicy {
    use TrackRenameCommandPolicy::{CommitThenRun, Edit, Swallow};
    match command_id {
        // Text edits act on the field however they were asked for.
        "edit:select-all" | "midi:select-all" | "automation:select-all-points" => {
            return Edit(TrackRenameEdit::SelectAll);
        }
        "edit:cut" => return Edit(TrackRenameEdit::Cut),
        "edit:copy" => return Edit(TrackRenameEdit::Copy),
        "edit:paste" => return Edit(TrackRenameEdit::Paste),
        // Undo, redo, saving and leaving always keep the name first, whatever
        // chord ran them (quit is Alt+F4 on Windows).
        "edit:undo" | "edit:redo" | "app:quit" => return CommitThenRun,
        id if is_file_command(id) => return CommitThenRun,
        _ => {}
    }
    match chord {
        None | Some(TrackRenameChord::Command) => CommitThenRun,
        Some(TrackRenameChord::WordLeft) => Edit(TrackRenameEdit::WordLeft),
        Some(TrackRenameChord::WordRight) => Edit(TrackRenameEdit::WordRight),
        Some(TrackRenameChord::Typing) => Swallow,
    }
}

/// The File menu's project and export commands.
fn is_file_command(command_id: &str) -> bool {
    const PREFIXES: [&str; 6] = [
        "file:",
        "project:save",
        "project:open",
        "project:new",
        "project:close",
        "project:recent",
    ];
    PREFIXES.iter().any(|prefix| command_id.starts_with(prefix))
}

/// Whether the header of the track at `index` is drawn this frame: the track
/// list draws only the rows in the visible window plus its overscan, and never
/// a zero-height (hidden or collapsed-folder) row.
pub(crate) fn track_header_rendered(
    row_layout: &TrackRowLayout,
    index: usize,
    scroll_y: f32,
    viewport_height: f32,
) -> bool {
    let Some(row) = row_layout.row_for_index(index) else {
        return false;
    };
    if row.height <= 0.0 {
        return false;
    }
    let (start, end, _, _) = crate::components::timeline::track_resize::visible_track_row_range(
        row_layout,
        scroll_y,
        viewport_height,
        crate::components::timeline::track_list::OVERSCAN,
    );
    (start..end).contains(&index)
}

impl Timeline {
    /// Where keyboard focus goes when an editor the arrangement owns closes.
    /// Set once by the owner to its shortcut anchor.
    pub fn set_focus_return(&mut self, handle: FocusHandle) {
        self.focus_return = Some(handle);
    }

    pub fn track_rename_open(&self) -> bool {
        self.track_rename.is_some()
    }

    /// Whether the rename field holds keyboard focus, so keys belong to it.
    pub fn track_rename_focused(&self, window: &Window) -> bool {
        self.track_rename
            .as_ref()
            .is_some_and(|session| session.input.is_focused(window))
    }

    /// Open the name editor on `track_id`'s header, with the whole name
    /// selected. A rename open on another track is committed first.
    pub(super) fn begin_track_rename(
        &mut self,
        track_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .track_rename
            .as_ref()
            .is_some_and(|session| session.track_id == track_id)
        {
            return;
        }
        self.end_track_rename(true, Some(&mut *window), cx);
        let Some(track) = self.state.find_track(track_id) else {
            return;
        };
        if is_arrangement_hidden_track(track) {
            return;
        }
        let mut input = TextInputState::new("track-header-rename", cx.focus_handle())
            .with_accessible_label("Track name")
            .with_field_height(TRACK_NAME_FIELD_HEIGHT);
        input.set_value(track.name.clone());
        input.select_all();
        // The row is on screen — it was just double-clicked — so the field can
        // take focus now rather than on its first frame.
        input.focus_handle.focus(window, cx);
        let blur = cx.on_blur(&input.focus_handle, window, |this, window, cx| {
            this.track_rename_blurred(window, cx)
        });
        self.track_rename = Some(TrackRenameSession {
            track_id: track_id.to_string(),
            input,
            window_handle: window.window_handle(),
            _blur: blur,
        });
        cx.notify();
    }

    /// Commit the open rename from outside the field: Tab, or an app command
    /// that commits first. `window` is `None` on the menu path, which runs
    /// while the window is busy dispatching; focus is handed back once it is
    /// free.
    pub fn commit_track_rename(
        &mut self,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.end_track_rename(true, window, cx)
    }

    /// Another project is replacing this arrangement: opened, switched to,
    /// rolled back to, a New Project or a close. Call it wherever the owner
    /// replaces [`TimelineState`], after the replacement.
    ///
    /// An open rename is dropped without recording anything. Its draft and
    /// its track id belong to the project going away, and track ids repeat
    /// across projects (`track-3` is in most of them), so a later commit would
    /// rename a track the user never touched. The names revision takes a fresh
    /// value, so every surface that copies names out of the timeline reloads
    /// even if the new arrangement happens to carry the value it last saw.
    pub fn end_track_rename_for_project_change(&mut self, cx: &mut Context<Self>) {
        self.end_track_rename(false, None, cx);
        self.state.touch_track_names();
        cx.notify();
    }

    /// End the session: record the rename when `commit` and the draft is a
    /// real change, then hand focus back if the field still has it. Idempotent,
    /// so the blur that follows an Enter or Escape does nothing.
    fn end_track_rename(
        &mut self,
        commit: bool,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.track_rename.take() else {
            return false;
        };
        if commit {
            // `prev` is the name the track has now, not the one the field
            // opened on: something may have renamed it meanwhile (a menu-bound
            // undo on macOS never passes through the field).
            if let Some(edit) =
                EditCommand::rename_track(&self.state, &session.track_id, &session.input.value)
            {
                self.run_edit_command(edit, cx);
            }
        }
        let TrackRenameSession {
            input,
            window_handle,
            ..
        } = session;
        let field_focus = input.focus_handle;
        match window {
            Some(window) => {
                if field_focus.is_focused(window) {
                    self.return_focus(window, cx);
                }
            }
            None => {
                let focus_return = self.focus_return.clone();
                cx.defer(move |cx| {
                    let _ = window_handle.update(cx, |_, window, cx| {
                        if field_focus.is_focused(window) {
                            match focus_return {
                                Some(handle) => handle.focus(window, cx),
                                None => window.blur(),
                            }
                        }
                    });
                });
            }
        }
        cx.notify();
        true
    }

    fn return_focus(&self, window: &mut Window, cx: &mut gpui::App) {
        match self.focus_return.as_ref() {
            Some(handle) => handle.focus(window, cx),
            // With no anchor, an empty focus is still better than a stale one:
            // the owner reclaims an empty focus on its next render.
            None => window.blur(),
        }
    }

    fn track_rename_blurred(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // An inactive window reports an empty focus path, which reads as a
        // blur. Switching apps or clicking a plug-in editor window keeps the
        // edit open; the field has focus again when the window comes back.
        if !window.is_window_active() {
            return;
        }
        self.end_track_rename(true, Some(window), cx);
    }

    /// A press outside the field. Committed at once, in the capture phase,
    /// before the press reaches what it landed on — a button acting on its
    /// mouse-down would otherwise meet a rename still open and have its command
    /// swallowed.
    fn track_rename_outside_press(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A press on the field's own Cut/Copy/Paste menu is part of the edit.
        if self
            .track_rename
            .as_ref()
            .is_some_and(|session| session.input.context_menu.is_some())
        {
            return;
        }
        self.end_track_rename(true, Some(window), cx);
    }

    /// Route one key to the open rename; see [`TrackRenameKeyOutcome`].
    pub(crate) fn handle_track_rename_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> TrackRenameKeyOutcome {
        let Some(session) = self.track_rename.as_mut() else {
            return TrackRenameKeyOutcome::PassCommand;
        };
        // Escape closes the field's own Cut/Copy/Paste menu before it can
        // cancel the rename.
        if session.input.context_menu.is_some() && event.keystroke.key == "escape" {
            session.input.context_menu = None;
            cx.notify();
            return TrackRenameKeyOutcome::Consumed;
        }
        // The IME path: printable text arrives through the input handler, so
        // inserting it here as well would double every character.
        let action = session.input.handle_key_ime(event, Some(cx));
        match track_rename_key_step(action, &event.keystroke.modifiers) {
            TrackRenameKeyStep::Commit => {
                self.end_track_rename(true, Some(window), cx);
                TrackRenameKeyOutcome::Finished
            }
            TrackRenameKeyStep::Cancel => {
                self.end_track_rename(false, Some(window), cx);
                TrackRenameKeyOutcome::Finished
            }
            TrackRenameKeyStep::Consume => {
                cx.notify();
                TrackRenameKeyOutcome::Consumed
            }
            TrackRenameKeyStep::PassToCommands => TrackRenameKeyOutcome::PassCommand,
        }
    }

    /// Perform a text edit an app command stands for; see
    /// [`TrackRenameCommandPolicy::Edit`].
    pub(crate) fn apply_track_rename_edit(
        &mut self,
        edit: TrackRenameEdit,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.track_rename.as_mut() else {
            return;
        };
        let input = &mut session.input;
        let _ = match edit {
            TrackRenameEdit::WordLeft => input.apply_event(TextInputEvent::MoveWordLeft, None),
            TrackRenameEdit::WordRight => input.apply_event(TextInputEvent::MoveWordRight, None),
            TrackRenameEdit::SelectAll => input.apply_context_command(TEXT_INPUT_SELECT_ALL, cx),
            TrackRenameEdit::Cut => input.apply_context_command(TEXT_INPUT_CUT, cx),
            TrackRenameEdit::Copy => input.apply_context_command(TEXT_INPUT_COPY, cx),
            TrackRenameEdit::Paste => input.apply_context_command(TEXT_INPUT_PASTE, cx),
        };
        cx.notify();
    }

    /// End a rename whose header is not drawn this frame: its track deleted,
    /// folded into a collapsed folder, or scrolled out of the rendered rows.
    /// The field would otherwise keep keyboard focus with no element to route
    /// keys through, and every shortcut would be dead until the next click.
    pub(super) fn end_track_rename_if_unrendered(
        &mut self,
        row_layout: &TrackRowLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.track_rename.as_ref() else {
            return;
        };
        let rendered = self
            .state
            .tracks
            .iter()
            .position(|track| track.id == session.track_id)
            .is_some_and(|index| {
                track_header_rendered(
                    row_layout,
                    index,
                    self.state.viewport.scroll_y,
                    self.state.viewport.viewport_height,
                )
            });
        if rendered {
            return;
        }
        // Committed like any other way out of the field; a deleted track
        // records nothing.
        self.end_track_rename(true, Some(&mut *window), cx);
        // The studio rendered before this view, so it only sees the commit on
        // the next frame. That frame notifies this view, which marks every
        // ancestor view dirty as well (`Window::mark_view_dirty`), so the
        // studio renders again: it takes the focus back and its
        // `sync_track_name_surfaces` refreshes the mixer tree, the pop-out
        // mixer and the built-in editor sidebars with the committed name.
        window.request_animation_frame();
    }

    /// The rename as the header draws it, or `None` when none is open.
    pub(super) fn track_rename_header(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<std::rc::Rc<TrackHeaderRename>> {
        let session = self.track_rename.as_ref()?;
        let timeline = cx.entity();
        Some(std::rc::Rc::new(TrackHeaderRename {
            track_id: session.track_id.clone(),
            input: session.input.clone(),
            focused: session.input.is_focused(window),
            callbacks: track_rename_field_callbacks(timeline.clone()),
            ime_target: timeline,
        }))
    }
}

/// Whether a pointer event on the rename field changed what it draws, from
/// its caret and selection before and after the event. A press always does:
/// it places the caret. A move changes the selection only while a drag
/// selection is under way (`TextInputState::handle_mouse_drag` ignores the
/// rest), and a release only when it ends one.
fn rename_field_pointer_redraws(
    phase: TextInputMousePhase,
    before: TextSelection,
    after: TextSelection,
) -> bool {
    matches!(phase, TextInputMousePhase::Down) || before != after
}

/// Mouse selection, the Cut/Copy/Paste menu and the outside-press commit for
/// the rename field, all routed to the session's input.
fn track_rename_field_callbacks(timeline: Entity<Timeline>) -> TextInputCallbacks {
    let mouse_target = timeline.clone();
    let menu_target = timeline.clone();
    let command_target = timeline.clone();
    TextInputCallbacks {
        on_mouse: Some(Arc::new(
            move |event: &TextInputMouseEvent, _window: &mut Window, cx: &mut gpui::App| {
                let _ = mouse_target.update(cx, |this, cx| {
                    let Some(session) = this.track_rename.as_mut() else {
                        return;
                    };
                    let before = session.input.selection();
                    match event.phase {
                        TextInputMousePhase::Down => {
                            session.input.handle_mouse_down(event.index, event.extend)
                        }
                        TextInputMousePhase::Drag => session.input.handle_mouse_drag(event.index),
                        TextInputMousePhase::Up => session.input.handle_mouse_up(),
                    }
                    // The field hears every move over it and every release in
                    // the window; only a press or a selection that moved
                    // redraws the arrangement.
                    if rename_field_pointer_redraws(event.phase, before, session.input.selection())
                    {
                        cx.notify();
                    }
                });
            },
        )),
        on_context_menu: Some(Arc::new(
            move |pos: &(f32, f32), window: &mut Window, cx: &mut gpui::App| {
                let (x, y) = *pos;
                let viewport = window.viewport_size();
                let clipboard_has_text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .is_some_and(|text| !text.is_empty());
                let _ = menu_target.update(cx, |this, cx| {
                    let Some(session) = this.track_rename.as_mut() else {
                        return;
                    };
                    session.input.context_menu = Some(TextContextMenuAnchor {
                        x,
                        y,
                        viewport_width: viewport.width.into(),
                        viewport_height: viewport.height.into(),
                        clipboard_has_text,
                    });
                    cx.notify();
                });
            },
        )),
        on_context_command: Some(Arc::new(
            move |command: Option<&str>, window: &mut Window, cx: &mut gpui::App| {
                let command = command.map(str::to_string);
                let _ = command_target.update(cx, |this, cx| {
                    let Some(session) = this.track_rename.as_mut() else {
                        return;
                    };
                    session.input.context_menu = None;
                    if let Some(command) = command {
                        let _ = session.input.apply_context_command(&command, cx);
                        // The press on the menu moved focus to the studio
                        // anchor; the edit is not over, so take it back.
                        session.input.focus_handle.focus(window, cx);
                    }
                    cx.notify();
                });
            },
        )),
        on_mouse_down_out: Some(Arc::new(move |window: &mut Window, cx: &mut gpui::App| {
            let _ = timeline.update(cx, |this, cx| this.track_rename_outside_press(window, cx));
        })),
    }
}

/// OS text input for the rename field — composition (Japanese, Chinese,
/// Korean), Thai and dead keys. Mirrors `impl_single_input_window_ime!`, routed
/// to the open session; with none open every call is a no-op.
impl EntityInputHandler for Timeline {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        self.track_rename
            .as_mut()?
            .input
            .text_for_utf16_range(range, actual_range)
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.track_rename
            .as_mut()?
            .input
            .selected_text_range_utf16(ignore_disabled_input)
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.track_rename.as_ref()?.input.marked_text_range_utf16()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.track_rename.as_mut() {
            session.input.unmark_text();
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.track_rename.as_mut() {
            session.input.replace_text_in_utf16_range(range, text);
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.track_rename.as_mut() {
            session
                .input
                .replace_and_mark_text_in_utf16_range(range, new_text, new_selected_range);
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.track_rename
            .as_mut()?
            .input
            .bounds_for_utf16_range(range_utf16, bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::text_input::TextEditBuffer;
    use crate::components::timeline::timeline_state::{CreateTrackOptions, TrackType};
    use gpui::{Keystroke, Modifiers};

    fn key(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: Keystroke {
                modifiers,
                key: key.to_string(),
                key_char: key_char.map(str::to_string),
            },
            is_held: false,
            prefer_character_input: false,
        }
    }

    fn cmd() -> Modifiers {
        Modifiers {
            platform: true,
            ..Default::default()
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    fn ctrl_shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..ctrl()
        }
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..Default::default()
        }
    }

    /// Run `event` through the field exactly as the rename does and report the
    /// step, plus the text the field holds afterwards.
    fn step_for(event: &KeyDownEvent) -> (TrackRenameKeyStep, String) {
        let mut field = TextEditBuffer::default();
        field.set_value("Bass");
        let action = field.handle_key_ime(event, None);
        (
            track_rename_key_step(action, &event.keystroke.modifiers),
            field.value.clone(),
        )
    }

    #[test]
    fn rename_key_table() {
        use TrackRenameKeyStep::{Cancel, Commit, Consume, PassToCommands};
        let none = Modifiers::default();
        let cases: [(KeyDownEvent, TrackRenameKeyStep); 12] = [
            (key("enter", None, none), Commit),
            (key("numpad_enter", None, none), Commit),
            (key("escape", None, none), Cancel),
            // Typing: consumed, and not inserted by the key — the IME inserts.
            (key("a", Some("a"), none), Consume),
            (key("space", Some(" "), none), Consume),
            (key("backspace", None, none), Consume),
            // Plain keys the field ignores never reach the shortcuts.
            (key("up", None, none), Consume),
            (key("f5", None, none), Consume),
            // The field's own chords.
            (key("a", None, cmd()), Consume),
            (key("left", None, alt()), Consume),
            // Chords it has no use for go to the command policy.
            (key("z", None, cmd()), PassToCommands),
            (key("delete", None, ctrl_shift()), PassToCommands),
        ];
        for (event, expected) in cases {
            let (step, _) = step_for(&event);
            assert_eq!(step, expected, "key {:?}", event.keystroke);
        }
    }

    #[test]
    fn a_typed_character_is_left_to_the_ime() {
        let (_, value) = step_for(&key("x", Some("x"), Modifiers::default()));
        assert_eq!(value, "Bass", "the key path must not insert; the IME does");
    }

    #[test]
    fn a_plain_or_function_chord_the_field_ignores_is_settled_by_modifiers() {
        let none = Modifiers::default();
        assert_eq!(
            track_rename_key_step(TextInputAction::Pass, &none),
            TrackRenameKeyStep::Consume
        );
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            track_rename_key_step(TextInputAction::Pass, &shift),
            TrackRenameKeyStep::Consume
        );
        for modifiers in [cmd(), ctrl(), alt()] {
            assert_eq!(
                track_rename_key_step(TextInputAction::Pass, &modifiers),
                TrackRenameKeyStep::PassToCommands
            );
        }
    }

    #[test]
    fn command_policy_table() {
        use TrackRenameChord::{Command, Typing, WordLeft, WordRight};
        use TrackRenameCommandPolicy::{CommitThenRun, Edit, Swallow};
        let cases = [
            // A Cmd/Ctrl chord is a shortcut: the name is kept, then it runs.
            ("edit:duplicate", Some(Command), CommitThenRun),
            ("track:delete", Some(Command), CommitThenRun),
            ("panel:toggle-mixer", Some(Command), CommitThenRun),
            ("track:add-midi", Some(Command), CommitThenRun),
            ("app:preferences", Some(Command), CommitThenRun),
            ("view:zoom-in", Some(Command), CommitThenRun),
            ("tools:command-palette", Some(Command), CommitThenRun),
            // So is a command no keystroke is bound to: only a click runs it.
            ("track:rename", None, CommitThenRun),
            ("transport:play-pause", None, CommitThenRun),
            // Saving, the File menu, undo, redo and quit always commit first,
            // whatever chord ran them.
            ("project:save", Some(Command), CommitThenRun),
            ("project:save-as", Some(Command), CommitThenRun),
            ("project:save-copy", Some(Command), CommitThenRun),
            ("project:open", Some(Command), CommitThenRun),
            ("project:new", Some(Command), CommitThenRun),
            ("project:close", Some(Command), CommitThenRun),
            ("project:recent:open", None, CommitThenRun),
            ("file:export-arrangement", Some(Command), CommitThenRun),
            ("file:export-midi", Some(Typing), CommitThenRun),
            ("edit:undo", Some(Command), CommitThenRun),
            ("edit:redo", Some(Typing), CommitThenRun),
            ("app:quit", Some(Typing), CommitThenRun),
            // Option+arrows move by word, whichever command they are bound to.
            (
                "transport:rewind",
                Some(WordLeft),
                Edit(TrackRenameEdit::WordLeft),
            ),
            (
                "transport:fast-forward",
                Some(WordRight),
                Edit(TrackRenameEdit::WordRight),
            ),
            (
                "midi:nudge-left",
                Some(WordLeft),
                Edit(TrackRenameEdit::WordLeft),
            ),
            // Select-all and the clipboard act on the field, however asked.
            (
                "edit:select-all",
                Some(Command),
                Edit(TrackRenameEdit::SelectAll),
            ),
            (
                "midi:select-all",
                Some(Command),
                Edit(TrackRenameEdit::SelectAll),
            ),
            (
                "automation:select-all-points",
                Some(Command),
                Edit(TrackRenameEdit::SelectAll),
            ),
            ("edit:cut", Some(Command), Edit(TrackRenameEdit::Cut)),
            ("edit:copy", None, Edit(TrackRenameEdit::Copy)),
            ("edit:paste", Some(Command), Edit(TrackRenameEdit::Paste)),
            // Option-only and bare keys are typing: nothing runs.
            ("view:toggle-virtual-keyboard", Some(Typing), Swallow),
            ("clip:split-at-playhead", Some(Typing), Swallow),
            ("transport:play-pause", Some(Typing), Swallow),
            ("transport:record", Some(Typing), Swallow),
            ("edit:delete", Some(Typing), Swallow),
            ("tools:select-cut", Some(Typing), Swallow),
            ("track:add-plugin", Some(Typing), Swallow),
        ];
        for (command, chord, expected) in cases {
            assert_eq!(
                track_rename_command_policy(command, chord),
                expected,
                "command {command} chord {chord:?}"
            );
        }
    }

    #[test]
    fn chords_from_accelerators_and_keystrokes() {
        use TrackRenameChord::{Command, Typing, WordLeft, WordRight};
        let accelerators = [
            ("Ctrl+D", Some(Command)),
            ("Ctrl+Shift+Delete", Some(Command)),
            ("Cmd+3", Some(Command)),
            ("Ctrl+Alt+E", Some(Command)),
            ("Ctrl+Alt+Shift+S", Some(Command)),
            ("Alt+Left", Some(WordLeft)),
            ("Alt+ArrowRight", Some(WordRight)),
            ("Alt+K", Some(Typing)),
            ("Alt+Shift+Left", Some(Typing)),
            ("Alt+F4", Some(Typing)),
            ("Space", Some(Typing)),
            ("Shift+Space", Some(Typing)),
            ("F5", Some(Typing)),
            ("", None),
        ];
        for (accelerator, expected) in accelerators {
            assert_eq!(
                TrackRenameChord::of_accelerator(accelerator),
                expected,
                "accelerator {accelerator:?}"
            );
        }
        let keystroke = |key: &str, modifiers: Modifiers| Keystroke {
            modifiers,
            key: key.to_string(),
            key_char: None,
        };
        let function = Modifiers {
            function: true,
            ..Default::default()
        };
        let keystrokes = [
            (keystroke("d", cmd()), Command),
            (keystroke("3", ctrl()), Command),
            (keystroke("delete", ctrl_shift()), Command),
            (keystroke("left", alt()), WordLeft),
            (keystroke("right", alt()), WordRight),
            (keystroke("k", alt()), Typing),
            (keystroke("f5", function), Typing),
        ];
        for (keystroke, expected) in keystrokes {
            assert_eq!(
                TrackRenameChord::of_keystroke(&keystroke),
                expected,
                "keystroke {keystroke:?}"
            );
        }
    }

    #[test]
    fn a_command_without_its_keystroke_is_judged_by_its_accelerators() {
        use TrackRenameChord::{Command, Typing, WordLeft};
        let likeliest =
            |accelerators: &[&str]| TrackRenameChord::likeliest(accelerators.iter().copied());
        // No accelerator at all: it can only have been a click.
        assert_eq!(likeliest(&[]), None);
        assert_eq!(likeliest(&["", "   "]), None);
        assert_eq!(likeliest(&["Ctrl+D"]), Some(Command));
        assert_eq!(likeliest(&["Alt+K"]), Some(Typing));
        assert_eq!(likeliest(&["R"]), Some(Typing));
        assert_eq!(likeliest(&["Alt+Left"]), Some(WordLeft));
        // A Cmd/Ctrl accelerator anywhere makes it a shortcut.
        assert_eq!(likeliest(&["Alt+K", "Ctrl+K"]), Some(Command));
        assert_eq!(likeliest(&["Ctrl+K", "Alt+K"]), Some(Command));
        // Otherwise a word move beats plain typing, in either order.
        assert_eq!(likeliest(&["Numpad-", "Alt+Left"]), Some(WordLeft));
        assert_eq!(likeliest(&["Alt+Left", "Numpad-"]), Some(WordLeft));
    }

    #[test]
    fn only_a_press_or_a_moved_selection_redraws_the_arrangement() {
        use TextInputMousePhase::{Down, Drag, Up};
        let caret = |at: usize| TextSelection {
            anchor: at,
            cursor: at,
        };
        let range = TextSelection {
            anchor: 1,
            cursor: 3,
        };
        // A press places the caret, even where it already was.
        assert!(rename_field_pointer_redraws(Down, caret(2), caret(2)));
        // Hovering, or a drag that lands on the same index, changes nothing.
        assert!(!rename_field_pointer_redraws(Drag, caret(2), caret(2)));
        assert!(!rename_field_pointer_redraws(Drag, range, range));
        assert!(rename_field_pointer_redraws(Drag, caret(1), range));
        // A release anywhere in the window, with no drag to end, is silent.
        assert!(!rename_field_pointer_redraws(Up, caret(4), caret(4)));
        assert!(!rename_field_pointer_redraws(Up, range, range));
    }

    fn tracks(count: usize) -> TimelineState {
        let mut state = TimelineState::default();
        for _ in 0..count {
            state.create_midi_track();
        }
        state
    }

    #[test]
    fn a_header_in_the_row_window_is_rendered() {
        let state = tracks(30);
        let layout = state.track_row_layout();
        assert!(track_header_rendered(&layout, 0, 0.0, 300.0));
        assert!(track_header_rendered(&layout, 3, 0.0, 300.0));
    }

    #[test]
    fn a_header_scrolled_out_of_the_row_window_is_not_rendered() {
        let state = tracks(30);
        let layout = state.track_row_layout();
        assert!(!track_header_rendered(&layout, 25, 0.0, 300.0));
        let far = layout.row_for_index(25).expect("row").y;
        assert!(!track_header_rendered(&layout, 0, far, 300.0));
        assert!(track_header_rendered(&layout, 25, far, 300.0));
        assert!(
            !track_header_rendered(&layout, 99, 0.0, 300.0),
            "no such row"
        );
    }

    #[test]
    fn a_header_folded_into_a_collapsed_folder_is_not_rendered() {
        let mut state = tracks(2);
        let group = state.create_track(CreateTrackOptions {
            track_type: TrackType::Group,
            name: "Folder".into(),
            color: crate::color::auto_color_for_index(0),
            volume: 0.8,
            pan: 0.0,
            armed: false,
            input_monitor: crate::project::InputMonitorMode::Off,
        });
        let child = state.tracks[1].id.clone();
        assert!(state.assign_track_to_group(&child, &group));
        let child_index = state
            .tracks
            .iter()
            .position(|track| track.id == child)
            .expect("child");
        assert!(track_header_rendered(
            &state.track_row_layout(),
            child_index,
            0.0,
            600.0
        ));
        assert!(state.toggle_group_collapsed(&group).is_some());
        assert!(!track_header_rendered(
            &state.track_row_layout(),
            child_index,
            0.0,
            600.0
        ));
    }
}

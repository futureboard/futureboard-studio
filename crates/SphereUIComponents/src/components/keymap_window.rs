//! Keymap / Keyboard Shortcuts editor dialog.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, svg, uniform_list, App, AppContext, Bounds, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::assets;
use crate::components::controls::{
    fb_badge, fb_button, fb_dock_tab, fb_dock_tab_strip, fb_field_label, fb_icon_button,
    fb_tooltip, FbButtonKind, FbDockPlanes,
};
use crate::components::form::select::{select, SelectOption};
use crate::components::key_recorder::{key_recorder_field, KeyRecorderState};
use crate::components::text_input::{
    bind_mouse_selection, text_field_with_callbacks, TextInputAction, TextInputState,
};
use crate::components::title_bar::external_window_titlebar;
use crate::keymap::{
    format_keystroke_list, format_keystrokes, KeymapConflict, KeymapManager, KeymapRow,
    KeymapScope, PROFILE_DESCRIPTORS,
};
use crate::theme::{self, radius, size, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const KEYMAP_WINDOW_WIDTH: f32 = 1200.0;
pub const KEYMAP_WINDOW_HEIGHT: f32 = 760.0;
pub const KEYMAP_WINDOW_MIN_WIDTH: f32 = 960.0;
pub const KEYMAP_WINDOW_MIN_HEIGHT: f32 = 620.0;

const ROW_H: f32 = 28.0;
const HEADER_H: f32 = 28.0;
const FOOTER_H: f32 = 26.0;
const TOPBAR_H: f32 = 44.0;
const SEARCH_W: f32 = 300.0;
/// Where the search field's text starts: past the glyph drawn over it.
const SEARCH_TEXT_INSET: f32 = 30.0;
const PROFILE_W: f32 = 150.0;
const KEYS_W: f32 = 240.0;
const SCOPE_W: f32 = 140.0;
const SOURCE_W: f32 = 140.0;
/// The selected row's leading accent edge.
const MARKER_W: f32 = 2.0;

const ICON_JSON_PATH: &str = "icons/menu/braces.svg";
const ICON_IMPORT_PATH: &str = "icons/menu/file-down.svg";
const ICON_EXPORT_PATH: &str = "icons/menu/file-up.svg";
const ICON_RESET_PATH: &str = "icons/menu/rotate-ccw.svg";

pub type KeymapChangedCb = Arc<dyn Fn(KeymapManager, &mut App) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Table,
    Json,
}

#[derive(Debug, Clone)]
struct EditDialogState {
    action_id: String,
    action_label: String,
    arguments_json: String,
    /// Where the binding answers. Chosen in the dialog; a new binding starts
    /// in the scope its action acts on.
    scope: KeymapScope,
    recorder: KeyRecorderState,
    conflicts: Vec<KeymapConflict>,
    show_conflict: bool,
    /// Filter over the action list, for a binding being created from nothing.
    ///
    /// Editing a row already knows its action. Creating one did not, and had no
    /// way to be told: the dialog printed "Enter action id in JSON editor for
    /// now" and saved an empty `action_id`, so the whole Create flow could not
    /// produce a binding however carefully the keystroke was recorded. This is
    /// the missing half.
    action_query: String,
}

impl EditDialogState {
    /// A dialog that knows its action from the row that opened it.
    fn for_action(
        action_id: String,
        action_label: String,
        arguments_json: String,
        scope: KeymapScope,
        captured: Option<String>,
    ) -> Self {
        Self {
            action_id,
            action_label,
            arguments_json,
            scope,
            recorder: KeyRecorderState {
                captured,
                ..KeyRecorderState::default()
            },
            conflicts: Vec::new(),
            show_conflict: false,
            action_query: String::new(),
        }
    }

    /// A dialog with no action yet: the user picks one from the list.
    fn for_new_binding() -> Self {
        Self {
            action_id: String::new(),
            action_label: "New binding".into(),
            arguments_json: String::new(),
            scope: KeymapScope::Global,
            recorder: KeyRecorderState::default(),
            conflicts: Vec::new(),
            show_conflict: false,
            action_query: String::new(),
        }
    }
}

pub struct KeymapWindow {
    manager: KeymapManager,
    on_changed: KeymapChangedCb,
    view: ViewMode,
    search_input: TextInputState,
    filter_query: String,
    filter_pending: String,
    /// Only this scope's rows, or every scope.
    scope_filter: Option<KeymapScope>,
    json_input: TextInputState,
    json_error: Option<String>,
    selected_row_id: Option<String>,
    profile_select_open: bool,
    edit_dialog: Option<EditDialogState>,
    create_dialog_open: bool,
    status_message: Option<String>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    /// Live only while the key recorder is armed. Keystroke interceptors run
    /// before binding resolution, so an armed recorder sees chords such as
    /// `ctrl-s` instead of losing them to the action they are already bound to.
    recorder_intercept: Option<gpui::Subscription>,
    /// The row the last plain click landed on, and when, for telling a
    /// double-click without relying on the platform's click count alone.
    last_row_click: Option<(String, std::time::Instant)>,
}

impl KeymapWindow {
    fn new(manager: KeymapManager, on_changed: KeymapChangedCb, cx: &mut Context<Self>) -> Self {
        let json = manager.json_text().unwrap_or_default();
        // Saved shortcuts that failed to load are reported, not hidden.
        let status_message = manager.overrides_issue().map(str::to_string);
        Self {
            manager,
            on_changed,
            view: ViewMode::Table,
            // The search glyph is drawn over the field's leading edge, so the
            // text starts after it.
            search_input: TextInputState::new("keymap-search", cx.focus_handle())
                .with_placeholder("Search actions, keys or scopes")
                .with_accessible_label("Search keybindings")
                .with_field_height(size::COMFORTABLE)
                .with_inline_text(SEARCH_TEXT_INSET, gpui::FontWeight::NORMAL),
            filter_query: String::new(),
            filter_pending: String::new(),
            scope_filter: None,
            json_input: TextInputState::new("keymap-json", cx.focus_handle())
                .with_accessible_label("Keymap JSON"),
            json_error: None,
            selected_row_id: None,
            profile_select_open: false,
            edit_dialog: None,
            create_dialog_open: false,
            status_message,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            recorder_intercept: None,
            last_row_click: None,
        }
        .with_json_text(json)
    }

    fn with_json_text(mut self, text: String) -> Self {
        self.json_input.set_value(text);
        self
    }

    /// Whether this press on `row_id` completes a double-click.
    ///
    /// The platform's `click_count` is taken from when each press is handled,
    /// not when it happened, so a slow frame after the first click can push
    /// the second one out of the double-click window — opening the editor
    /// took several tries with the mouse while Enter always worked. The same
    /// row pressed again within the interval counts on its own.
    fn is_second_click(&mut self, row_id: &str, click_count: usize) -> bool {
        const DOUBLE_CLICK: Duration = Duration::from_millis(600);
        let now = std::time::Instant::now();
        let repeat = self
            .last_row_click
            .as_ref()
            .is_some_and(|(id, at)| id == row_id && now.duration_since(*at) <= DOUBLE_CLICK);
        if click_count >= 2 || repeat {
            self.last_row_click = None;
            return true;
        }
        self.last_row_click = Some((row_id.to_string(), now));
        false
    }

    fn visible_rows(&self) -> Vec<KeymapRow> {
        self.manager
            .filtered_rows(&self.filter_query)
            .into_iter()
            .filter(|row| self.scope_filter.is_none_or(|scope| row.scope == scope))
            .cloned()
            .collect()
    }

    fn publish_changes(&self, cx: &mut App) {
        (self.on_changed)(self.manager.clone(), cx);
    }

    fn schedule_filter_debounce(&mut self, cx: &mut Context<Self>) {
        self.filter_pending = self.search_input.value.clone();
        let entity = cx.entity().clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            let _ = entity.update(cx, |this, cx| {
                if this.filter_pending == this.search_input.value {
                    this.filter_query = this.search_input.value.clone();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn switch_profile(&mut self, profile_id: String, cx: &mut Context<Self>) {
        if self.manager.is_dirty() {
            self.status_message = Some("Save or discard changes before switching profile.".into());
            cx.notify();
            return;
        }
        if let Err(error) = self.manager.set_active_profile(&profile_id) {
            self.status_message = Some(error);
        } else {
            self.status_message = None;
            self.publish_changes(cx);
        }
        self.profile_select_open = false;
        cx.notify();
    }

    fn open_edit_for_row(&mut self, row: &KeymapRow, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_dialog = Some(EditDialogState::for_action(
            row.action_id.clone(),
            row.action_label.clone(),
            row.arguments_json.clone().unwrap_or_default(),
            row.scope,
            row.keystrokes.first().cloned(),
        ));
        // Armed on open. Recording used to need a second, separate click on the
        // keystroke field — a small target inside a dialog the user had just
        // opened *to* record something — and if that click missed, every key
        // went to the window's own shortcuts instead. Opening the dialog is the
        // request to record; the field stays clickable to re-arm after a
        // mistake.
        self.arm_recorder(window, cx);
    }

    fn arm_recorder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.edit_dialog.as_mut() else {
            return;
        };
        dialog.recorder.arm();
        // The dialog has no inner focusable, so key events only reach this view
        // while the window root holds focus.
        self.focus_handle.focus(window, cx);

        let target = window.window_handle();
        let entity = cx.entity().downgrade();
        self.recorder_intercept = Some(cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle() != target {
                return;
            }
            let Some(entity) = entity.upgrade() else {
                return;
            };
            let consumed =
                entity.update(cx, |this, cx| this.record_keystroke(&event.keystroke, cx));
            if consumed {
                cx.stop_propagation();
            }
        }));
        cx.notify();
    }

    fn disarm_recorder(&mut self) {
        self.recorder_intercept = None;
        if let Some(dialog) = self.edit_dialog.as_mut() {
            dialog.recorder.disarm();
        }
    }

    /// Feed one intercepted keystroke to the armed recorder. Returns `true` when
    /// the recorder took it, so the caller can stop action dispatch.
    fn record_keystroke(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) -> bool {
        let Some(dialog) = self.edit_dialog.as_mut() else {
            return false;
        };
        let consumed = dialog.recorder.handle_keystroke(keystroke);
        let still_armed = dialog.recorder.armed;
        if consumed {
            if !still_armed {
                self.recorder_intercept = None;
            }
            cx.notify();
        }
        consumed
    }

    fn close_edit_dialog(&mut self, cx: &mut Context<Self>) {
        self.recorder_intercept = None;
        self.edit_dialog = None;
        cx.notify();
    }

    fn save_edit_dialog(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(mut dialog) = self.edit_dialog.take() else {
            return;
        };
        self.recorder_intercept = None;
        dialog.recorder.disarm();
        let action_id = dialog.action_id.clone();
        if action_id.trim().is_empty() {
            self.status_message = Some("Choose an action for this keybinding.".into());
            self.edit_dialog = Some(dialog);
            cx.notify();
            return;
        }
        let keys = dialog
            .recorder
            .captured
            .clone()
            .map(|key| vec![key])
            .unwrap_or_default();
        if keys.is_empty() {
            self.status_message = Some("Assign a keystroke before saving.".into());
            self.edit_dialog = Some(dialog);
            cx.notify();
            return;
        }
        match self
            .manager
            .tap_binding(&action_id, keys, dialog.scope, None, force)
        {
            Ok(conflicts) if !conflicts.is_empty() && !force => {
                let mut restored = dialog;
                restored.conflicts = conflicts;
                restored.show_conflict = true;
                self.edit_dialog = Some(restored);
            }
            Ok(_) => {
                let saved = self.manager.save_changes();
                self.json_input
                    .set_value(self.manager.json_text().unwrap_or_default());
                self.publish_changes(cx);
                self.status_message = Some(match saved {
                    Ok(()) => "Keybinding saved.".into(),
                    Err(error) => error,
                });
            }
            Err(error) => {
                self.status_message = Some(error);
                self.edit_dialog = Some(dialog);
            }
        }
        cx.notify();
    }

    fn import_profile(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            let entity = cx.entity().clone();
            cx.spawn(async move |_, cx| {
                let result = rfd::AsyncFileDialog::new()
                    .set_title("Import Keymap")
                    .add_filter("JSON", &["json"])
                    .pick_file()
                    .await;
                if let Some(handle) = result {
                    let path = handle.path().to_path_buf();
                    let _ = entity.update(cx, |this, cx| {
                        match this.manager.import_profile(&path) {
                            Ok(()) => {
                                let saved = this.manager.save_changes();
                                this.json_input
                                    .set_value(this.manager.json_text().unwrap_or_default());
                                this.publish_changes(cx);
                                this.status_message = Some(match saved {
                                    Ok(()) => format!("Imported {}", path.display()),
                                    Err(error) => error,
                                });
                            }
                            Err(error) => this.status_message = Some(error),
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        }

        #[cfg(not(feature = "native-dialogs"))]
        {
            self.status_message = Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    fn export_profile(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            let entity = cx.entity().clone();
            let default_name = format!("{}.json", self.manager.active_profile_id());
            cx.spawn(async move |_, cx| {
                let result = rfd::AsyncFileDialog::new()
                    .set_title("Export Keymap")
                    .set_file_name(&default_name)
                    .add_filter("JSON", &["json"])
                    .save_file()
                    .await;
                if let Some(handle) = result {
                    let path = handle.path().to_path_buf();
                    let _ = entity.update(cx, |this, cx| {
                        match this.manager.export_active_profile(&path) {
                            Ok(()) => {
                                this.status_message =
                                    Some(format!("Exported to {}", path.display()))
                            }
                            Err(error) => this.status_message = Some(error),
                        }
                        cx.notify();
                    });
                }
            })
            .detach();
        }

        #[cfg(not(feature = "native-dialogs"))]
        {
            self.status_message = Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    fn apply_json(&mut self, cx: &mut Context<Self>) {
        let text = self.json_input.value.clone();
        match self.manager.load_json_text(&text) {
            Ok(()) => match self.manager.save_changes() {
                Ok(()) => {
                    self.json_error = None;
                    self.publish_changes(cx);
                    self.status_message = Some("Keymap JSON reloaded.".into());
                }
                Err(error) => self.json_error = Some(error),
            },
            Err(error) => self.json_error = Some(error),
        }
        cx.notify();
    }

    /// Between the table and the profile's JSON. Opening the JSON reloads it
    /// from the keymap, so it never shows text older than the table.
    fn toggle_json_view(&mut self, cx: &mut Context<Self>) {
        self.view = match self.view {
            ViewMode::Table => {
                self.json_input
                    .set_value(self.manager.json_text().unwrap_or_default());
                ViewMode::Json
            }
            ViewMode::Json => ViewMode::Table,
        };
        cx.notify();
    }

    /// Put a changed row back to its profile's key. Rows that were never
    /// changed have nothing to reset and are left alone.
    fn reset_row(&mut self, row_id: &str, cx: &mut Context<Self>) {
        let overridden = self
            .manager
            .rows()
            .iter()
            .find(|row| row.id == row_id && row.is_user_override)
            .map(|row| (row.action_id.clone(), row.scope));
        let Some((action, scope)) = overridden else {
            return;
        };
        self.manager.reset_binding(&action, scope);
        self.status_message = match self.manager.save_changes() {
            Ok(()) => Some("Keybinding reset.".into()),
            Err(error) => Some(error),
        };
        self.json_input
            .set_value(self.manager.json_text().unwrap_or_default());
        self.publish_changes(cx);
        cx.notify();
    }

    /// Types into the create dialog's action filter.
    ///
    /// The dialog is modal and has no text input of its own to focus, so the
    /// window takes the keys itself: printable characters extend the query,
    /// Backspace shortens it. Only reached while an action still has to be
    /// chosen — once one is, every key belongs to the recorder again.
    fn type_into_action_filter(&mut self, event: &KeyDownEvent) -> bool {
        let Some(dialog) = self.edit_dialog.as_mut() else {
            return false;
        };
        if !dialog.action_id.is_empty() || dialog.recorder.armed {
            return false;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return false;
        }
        match event.keystroke.key.as_str() {
            "backspace" => {
                dialog.action_query.pop();
                true
            }
            key => {
                let text = event
                    .keystroke
                    .key_char
                    .as_deref()
                    .filter(|c| !c.is_empty() && !c.chars().any(char::is_control))
                    .map(str::to_string)
                    .or_else(|| (key.chars().count() == 1).then(|| key.to_string()));
                match text {
                    Some(text) => {
                        dialog.action_query.push_str(&text);
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// Actions offered by the create dialog, newest match first.
    fn action_choices(&self, query: &str) -> Vec<(String, String)> {
        let mut seen = std::collections::HashSet::new();
        self.manager
            .filtered_rows(query)
            .into_iter()
            .filter(|row| seen.insert(row.action_id.clone()))
            .take(ACTION_CHOICE_LIMIT)
            .map(|row| (row.action_id.clone(), row.action_label.clone()))
            .collect()
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit_dialog.is_some() {
            if let Some(dialog) = self.edit_dialog.as_mut() {
                if dialog.recorder.handle_key(event) {
                    cx.notify();
                    return;
                }
            }
            let key = event.keystroke.key.as_str();
            if key == "escape" {
                self.close_edit_dialog(cx);
                return;
            }
            if matches!(key, "enter" | "numpad_enter") {
                self.save_edit_dialog(false, cx);
                return;
            }
        }

        if self.view == ViewMode::Json && self.json_input.is_focused(window) {
            let action = self.json_input.handle_key_with_clipboard(event, Some(cx));
            if matches!(action, TextInputAction::Submit) {
                self.apply_json(cx);
            }
            cx.notify();
            return;
        }

        if self.search_input.is_focused(window) {
            let action = self.search_input.handle_key_with_clipboard(event, Some(cx));
            if matches!(action, TextInputAction::Cancel) {
                self.search_input.set_value("");
                self.filter_query.clear();
            } else {
                self.schedule_filter_debounce(cx);
            }
            cx.notify();
            return;
        }

        // A create dialog waiting for an action takes plain typing as its
        // filter before anything else looks at the key.
        if self.type_into_action_filter(event) {
            cx.notify();
            return;
        }
        let ctrl = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        let key = event.keystroke.key.as_str();
        match (ctrl, key) {
            (true, "f") => {
                self.search_input.focus_handle.focus(window, cx);
                cx.notify();
            }
            (true, "e") => self.toggle_json_view(cx),
            (true, "k") => {
                self.create_dialog_open = true;
                self.edit_dialog = Some(EditDialogState::for_new_binding());
                self.arm_recorder(window, cx);
            }
            _ => {
                if key == "escape" {
                    if !self.filter_query.is_empty() {
                        self.search_input.set_value("");
                        self.filter_query.clear();
                    } else if self.view == ViewMode::Json {
                        self.view = ViewMode::Table;
                    } else {
                        window.remove_window();
                    }
                    cx.notify();
                } else if key == "enter" || key == "numpad_enter" {
                    if let Some(id) = self.selected_row_id.clone() {
                        if let Some(row) = self.manager.rows().iter().find(|r| r.id == id) {
                            let row = row.clone();
                            self.open_edit_for_row(&row, window, cx);
                        }
                    }
                } else if matches!(key, "delete" | "backspace") {
                    if let Some(id) = self.selected_row_id.clone() {
                        self.reset_row(&id, cx);
                    }
                }
            }
        }
    }
}

impl Render for KeymapWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Nothing in this window is focused on open, and GPUI dispatches key
        // events only along the focused node's path, so the root has to claim
        // focus or `on_key_down` below never runs.
        if window.focused(cx).is_none() {
            self.focus_handle.focus(window, cx);
        }

        let rows = Arc::new(self.visible_rows());
        let row_count = rows.len();
        let selected = self.selected_row_id.clone();
        let entity = cx.entity().clone();
        let search_callbacks = bind_mouse_selection(entity.clone(), |this| &mut this.search_input);
        let json_callbacks = bind_mouse_selection(entity.clone(), |this| &mut this.json_input);
        let json_view_open = self.view == ViewMode::Json;

        let on_profile_toggle = {
            let entity = entity.clone();
            Arc::new(move |_: &(), _w: &mut Window, cx: &mut App| {
                let _ = entity.update(cx, |this, cx| {
                    this.profile_select_open = !this.profile_select_open;
                    cx.notify();
                });
            })
        };
        let on_profile_change = {
            let entity = entity.clone();
            Arc::new(move |id: &String, _w: &mut Window, cx: &mut App| {
                let _ = entity.update(cx, |this, cx| {
                    this.switch_profile(id.clone(), cx);
                });
            })
        };

        let profile_options: Vec<SelectOption> = PROFILE_DESCRIPTORS
            .iter()
            .map(|p| SelectOption::new(p.id, p.label))
            .collect();

        // One row: find on the left, the profile and the file actions on the
        // right, and the one primary action last. Import, export and the JSON
        // view are quiet glyphs with their names in tooltips — they are
        // occasional, and four labelled buttons in a row made each of them
        // shout as loudly as "New Binding".
        let search_focused = self.search_input.is_focused(window);
        let top_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .h(px(TOPBAR_H))
            .px(px(space::LOOSE))
            .bg(header_plane())
            .child(
                div()
                    .relative()
                    .w(px(SEARCH_W))
                    .flex_shrink_0()
                    .child(text_field_with_callbacks(
                        &self.search_input,
                        search_focused,
                        search_callbacks,
                    ))
                    .child(
                        div()
                            .absolute()
                            .left(px(space::LOOSE - 2.0))
                            .top_0()
                            .bottom_0()
                            .flex()
                            .items_center()
                            .child(
                                svg()
                                    .path(assets::ICON_SEARCH_PATH)
                                    .size(px(typography::UI_SM))
                                    .text_color(if search_focused {
                                        Colors::text_secondary()
                                    } else {
                                        Colors::text_faint()
                                    }),
                            ),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(typography::UI_XS))
                    .text_color(Colors::text_faint())
                    .child("Profile"),
            )
            .child(div().w(px(PROFILE_W)).flex_shrink_0().child(select(
                "keymap-profile",
                Some(self.manager.active_profile_id()),
                "Profile",
                profile_options,
                self.profile_select_open,
                false,
                on_profile_toggle,
                on_profile_change,
            )))
            .child(toolbar_divider())
            .child(toolbar_icon(
                "keymap-edit-json",
                ICON_JSON_PATH,
                if json_view_open {
                    "Back to the table  Ctrl+E"
                } else {
                    "Edit as JSON  Ctrl+E"
                },
                Some(json_view_open),
                {
                    let entity = entity.clone();
                    move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| this.toggle_json_view(cx));
                    }
                },
            ))
            .child(toolbar_icon(
                "keymap-import",
                ICON_IMPORT_PATH,
                "Import a keymap…",
                None,
                {
                    let entity = entity.clone();
                    move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| this.import_profile(cx));
                    }
                },
            ))
            .child(toolbar_icon(
                "keymap-export",
                ICON_EXPORT_PATH,
                "Export this profile…",
                None,
                {
                    let entity = entity.clone();
                    move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| this.export_profile(cx));
                    }
                },
            ))
            .child(toolbar_divider())
            .child(
                div()
                    .id("keymap-create-tip")
                    .tooltip(fb_tooltip("New keybinding  Ctrl+K"))
                    .child(fb_button(
                        "keymap-create",
                        "New Binding",
                        FbButtonKind::Primary,
                        true,
                        {
                            let entity = entity.clone();
                            move |_, window: &mut Window, cx: &mut App| {
                                let _ = entity.update(cx, |this, cx| {
                                    this.create_dialog_open = true;
                                    this.edit_dialog = Some(EditDialogState::for_new_binding());
                                    // Arm here as well as on Ctrl+K. The button and the
                                    // shortcut open the same dialog, and only one of
                                    // them used to start recording — so whether the
                                    // keystroke field listened depended on how you had
                                    // opened it.
                                    this.arm_recorder(window, cx);
                                });
                            }
                        },
                    )),
            );

        // Scopes are the table's tabs: the active one is cut out of the
        // header plane and runs into the table, so which slice of the keymap
        // is showing reads as where you are rather than as a filter setting.
        let planes = FbDockPlanes {
            strip: header_plane(),
            body: Colors::surface_base(),
        };
        let conflicts = self.manager.conflict_count();
        let scope_strip = if json_view_open {
            fb_dock_tab_strip(planes).child(fb_dock_tab(
                "keymap-json-tab",
                format!("{} profile as JSON", self.manager.active_profile_label()),
                None,
                true,
                planes,
                |_, _, _| {},
                None,
            ))
        } else {
            fb_dock_tab_strip(planes).children(
                std::iter::once(None)
                    .chain(KeymapScope::ALL.into_iter().map(Some))
                    .map(|scope| {
                        let entity = entity.clone();
                        fb_dock_tab(
                            gpui::SharedString::from(format!(
                                "keymap-scope-filter-{}",
                                scope.map_or("all", KeymapScope::key)
                            )),
                            scope.map_or("All", KeymapScope::label),
                            None,
                            self.scope_filter == scope,
                            planes,
                            move |_, _, cx| {
                                let _ = entity.update(cx, |this, cx| {
                                    this.scope_filter = scope;
                                    cx.notify();
                                });
                            },
                            None,
                        )
                    }),
            )
        }
        .child(
            div()
                .ml_auto()
                .h_full()
                .flex()
                .items_center()
                .pr(px(space::TIGHT))
                .when(conflicts > 0, |el| {
                    el.gap(px(space::SNUG))
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::status_warning())
                        .child(
                            svg()
                                .path(assets::ICON_TRIANGLE_ALERT_PATH)
                                .size(px(typography::UI_XS))
                                .text_color(Colors::status_warning()),
                        )
                        .child(if conflicts == 1 {
                            "1 conflict".to_string()
                        } else {
                            format!("{conflicts} conflicts")
                        })
                }),
        );

        let header = keymap_table_header();

        let rows_for_list = rows.clone();
        let list_entity = entity.clone();
        let scroll_for_thumb = self.scroll.0.borrow().base_handle.clone();
        let table_body = uniform_list("keymap-rows", row_count, move |range, _window, _cx| {
            range
                .map(|index| {
                    let row = &rows_for_list[index];
                    let selected = selected.as_deref() == Some(row.id.as_str());
                    let entity = list_entity.clone();
                    let row_clone = row.clone();
                    let edit_entity = entity.clone();
                    let edit_row = row.clone();
                    // The keystroke cell is the thing being changed, so one
                    // click on it opens the editor. A double-click anywhere
                    // else on the row does too.
                    let on_keystrokes =
                        move |_: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
                            cx.stop_propagation();
                            let _ = edit_entity.update(cx, |this, cx| {
                                this.selected_row_id = Some(edit_row.id.clone());
                                this.open_edit_for_row(&edit_row, window, cx);
                            });
                        };
                    let reset_entity = entity.clone();
                    let reset_id = row.id.clone();
                    let on_reset = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                        cx.stop_propagation();
                        let _ = reset_entity.update(cx, |this, cx| this.reset_row(&reset_id, cx));
                    };
                    keymap_row_element(row, selected, on_keystrokes, on_reset).on_mouse_down(
                        MouseButton::Left,
                        move |event, window, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                if this.is_second_click(&row_clone.id, event.click_count) {
                                    this.open_edit_for_row(&row_clone, window, cx);
                                } else {
                                    this.selected_row_id = Some(row_clone.id.clone());
                                    cx.notify();
                                }
                            });
                        },
                    )
                })
                .collect()
        })
        .size_full()
        .track_scroll(&self.scroll);

        let empty_state = (row_count == 0).then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(space::TIGHT))
                .child(
                    div()
                        .text_size(px(typography::UI_SM))
                        .text_color(Colors::text_secondary())
                        .child(if self.filter_query.trim().is_empty() {
                            "No keybindings in this scope".to_string()
                        } else {
                            format!("Nothing matches “{}”", self.filter_query.trim())
                        }),
                )
                .child(
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_faint())
                        .child("Try another scope, or search by action, key or scope name."),
                )
        });

        let table_view = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .bg(Colors::surface_base())
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .child(table_body)
                    .children(crate::components::scrollbar::scroll_handle_scrollbar(
                        "keymap-table-scrollbar",
                        &scroll_for_thumb,
                    ))
                    .children(empty_state),
            );

        // Built only while it is shown: the field carries the whole keymap as
        // text, and rebuilding it on every frame of the table view made each
        // click on a row pay for it.
        let json_view = json_view_open.then(|| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .bg(Colors::surface_base())
                .px(px(space::LOOSE))
                .py(px(space::LOOSE))
                .gap(px(space::BASE))
                .child(div().flex_1().min_h_0().child(text_field_with_callbacks(
                    &self.json_input,
                    self.json_input.is_focused(window),
                    json_callbacks,
                )))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::BASE))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(typography::UI_XS))
                                .text_color(if self.json_error.is_some() {
                                    Colors::status_error()
                                } else {
                                    Colors::text_faint()
                                })
                                .child(self.json_error.clone().unwrap_or_else(|| {
                                    "Changes take effect when applied. Esc returns to the table."
                                        .to_string()
                                })),
                        )
                        .child(fb_button(
                            "keymap-json-apply",
                            "Apply",
                            FbButtonKind::Primary,
                            true,
                            {
                                let entity = entity.clone();
                                move |_, _, cx| {
                                    let _ = entity.update(cx, |this, cx| this.apply_json(cx));
                                }
                            },
                        )),
                )
        });

        let total = self.manager.rows().len();
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .h(px(FOOTER_H))
            .px(px(space::LOOSE))
            .bg(header_plane())
            .text_size(px(typography::UI_XS))
            .text_color(Colors::text_faint())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(match self.status_message.as_ref() {
                        Some(message) => div()
                            .text_color(Colors::status_warning())
                            .child(message.clone()),
                        None => div().child(
                            "Click a shortcut or double-click a row to change it · Delete resets a changed one",
                        ),
                    }),
            )
            .child(div().flex_shrink_0().child(if row_count == total {
                format!("{total} actions")
            } else {
                format!("{row_count} of {total} actions")
            }));

        let action_choices = self
            .edit_dialog
            .as_ref()
            .filter(|dialog| dialog.action_id.is_empty())
            .map(|dialog| self.action_choices(&dialog.action_query))
            .unwrap_or_default();
        let edit_overlay = self
            .edit_dialog
            .as_ref()
            .map(|dialog| edit_dialog_overlay(dialog, action_choices, entity.clone()));

        let close_target = entity.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .child(external_window_titlebar("Keymap", "keymap-window-close", {
                let target = close_target.clone();
                move |window, cx| {
                    let _ = target.update(cx, |_, cx| cx.notify());
                    window.remove_window();
                }
            }))
            .child(top_bar)
            .child(scope_strip)
            .child(match json_view {
                Some(json_view) => json_view.into_any_element(),
                None => table_view.into_any_element(),
            })
            .child(footer)
            .children(edit_overlay)
    }
}

/// The plane the toolbar, the scope tabs and the footer share: the titlebar's
/// own, so the window's chrome reads as one band above and one below the
/// table instead of a stack of separately ruled strips.
fn header_plane() -> gpui::Rgba {
    Colors::surface_panel_alt()
}

/// A hairline between toolbar groups.
fn toolbar_divider() -> impl IntoElement {
    div()
        .w(px(1.0))
        .h(px(size::MICRO))
        .mx(px(space::TIGHT))
        .bg(Colors::border_subtle())
}

/// A quiet toolbar glyph whose name lives in its tooltip.
fn toolbar_icon(
    id: &'static str,
    icon: &'static str,
    tooltip: &'static str,
    toggled: Option<bool>,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(gpui::SharedString::from(format!("{id}-tip")))
        .tooltip(fb_tooltip(tooltip))
        .child(fb_icon_button(
            id,
            icon,
            tooltip,
            size::COMFORTABLE,
            toggled,
            on_click,
        ))
}

fn keymap_table_header() -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(HEADER_H))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pl(px(space::LOOSE + MARKER_W))
                .child("Action"),
        )
        .child(column(KEYS_W).child("Shortcut"))
        .child(column(SCOPE_W).child("Scope"))
        .child(column(SOURCE_W).pr(px(space::LOOSE)).child("Source"))
}

/// A fixed-width table column. The Action column takes the rest.
fn column(width: f32) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .w(px(width))
        .px(px(space::BASE))
        .overflow_hidden()
}

/// One pill in a row of scope choices — the scope picker in the edit dialog.
fn scope_chip(
    id: gpui::SharedString,
    label: &'static str,
    active: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let rest = if active {
        Colors::accent_soft()
    } else {
        Colors::with_alpha(Colors::surface_base(), 0.0)
    };
    let hover = Colors::composite(
        if active { rest } else { Colors::surface_base() },
        Colors::state_hover(),
    );
    div()
        .id(id)
        .flex()
        .items_center()
        .h(px(22.0))
        .px(px(8.0))
        .rounded(px(crate::theme::radius::PILL))
        .border(px(1.0))
        .border_color(if active {
            Colors::accent_primary()
        } else {
            Colors::border_subtle()
        })
        .bg(rest)
        .text_size(px(11.0))
        .text_color(if active {
            Colors::text_primary()
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .on_click(on_click)
        .child(label)
}

/// A key combination drawn as a keycap.
fn keycap(text: String) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .flex_shrink_0()
        .h(px(size::DENSE))
        .px(px(space::SNUG))
        .rounded(px(radius::CONTROL_SM))
        .bg(Colors::surface_panel_raised())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_primary())
        .child(text)
}

/// An action's menu path split into where it lives and what it is:
/// `Audio › Tap Tempo › Tap` → (`Audio › Tap Tempo › `, `Tap`).
fn split_action_label(label: &str) -> (&str, &str) {
    const SEP: &str = " › ";
    match label.rfind(SEP) {
        Some(at) => (&label[..at + SEP.len()], &label[at + SEP.len()..]),
        None => ("", label),
    }
}

fn keymap_row_element(
    row: &KeymapRow,
    selected: bool,
    on_keystrokes: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
    on_reset: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let base = Colors::surface_base();
    let rest = if selected {
        Colors::composite(base, Colors::state_selected())
    } else {
        base
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let (path, name) = split_action_label(&row.action_label);
    let keys = format_keystrokes(&row.keystrokes);

    let source = if row.is_conflict {
        div()
            .id(gpui::SharedString::from(format!(
                "keymap-conflict-{}",
                row.id
            )))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::TIGHT))
            .text_color(Colors::status_warning())
            .tooltip(fb_tooltip(format!(
                "Conflicts with {}",
                row.conflict_with.join(", ")
            )))
            .child(
                svg()
                    .path(assets::ICON_TRIANGLE_ALERT_PATH)
                    .size(px(typography::UI_XS))
                    .text_color(Colors::status_warning()),
            )
            .child("Conflict")
            .into_any_element()
    } else if row.is_user_override {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::TIGHT))
            .child(fb_badge("Changed", Colors::accent_primary()))
            .child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "keymap-reset-tip-{}",
                        row.id
                    )))
                    .tooltip(fb_tooltip("Reset to the profile's key"))
                    .child(fb_icon_button(
                        gpui::SharedString::from(format!("keymap-reset-{}", row.id)),
                        ICON_RESET_PATH,
                        "Reset to the profile's key",
                        size::MICRO,
                        None,
                        on_reset,
                    )),
            )
            .into_any_element()
    } else {
        div()
            .text_color(Colors::text_faint())
            .child(row.source.label())
            .into_any_element()
    };

    div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(ROW_H))
        .bg(rest)
        .hover(move |style| style.bg(hover))
        .text_size(px(typography::UI_SM))
        .cursor(gpui::CursorStyle::PointingHand)
        .when(selected, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(MARKER_W))
                    .bg(Colors::accent_primary()),
            )
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_1()
                .min_w_0()
                .pl(px(space::LOOSE + MARKER_W))
                .pr(px(space::BASE))
                .overflow_hidden()
                .child(
                    div()
                        .flex_shrink(1.0)
                        .min_w_0()
                        .truncate()
                        .text_color(Colors::text_faint())
                        .child(path.to_string()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(if row.enabled {
                            Colors::text_primary()
                        } else {
                            Colors::text_muted()
                        })
                        .child(name.to_string()),
                ),
        )
        .child(
            column(KEYS_W)
                .h_full()
                .gap(px(space::TIGHT))
                .on_mouse_down(MouseButton::Left, on_keystrokes)
                .when(keys.is_empty(), |el| {
                    el.text_size(px(typography::UI_XS))
                        .text_color(Colors::text_disabled())
                        .child("Unassigned")
                })
                .children(keys.into_iter().map(keycap)),
        )
        .child(
            column(SCOPE_W)
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(row.scope.label()),
        )
        .child(
            column(SOURCE_W)
                .pr(px(space::LOOSE))
                .text_size(px(typography::UI_XS))
                .child(source),
        )
}

/// How many actions the create dialog offers at once. The list is filtered by
/// typing, so this is a ceiling on the scroll, not on what is reachable.
const ACTION_CHOICE_LIMIT: usize = 40;

/// The action list a new binding is chosen from.
fn action_picker(
    dialog: &EditDialogState,
    choices: Vec<(String, String)>,
    entity: gpui::Entity<KeymapWindow>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .flex()
                .items_center()
                .h(px(26.0))
                .px(px(8.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::surface_input())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .text_size(px(11.0))
                .text_color(if dialog.action_query.is_empty() {
                    Colors::text_muted()
                } else {
                    Colors::text_primary()
                })
                .child(if dialog.action_query.is_empty() {
                    "Type to find an action…".to_string()
                } else {
                    dialog.action_query.clone()
                }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .max_h(px(150.0))
                .id("keymap-action-choices")
                .overflow_y_scroll()
                .rounded(px(crate::theme::radius::CONTROL))
                .bg(Colors::surface_card())
                .children(choices.into_iter().map(|(action_id, label)| {
                    let entity = entity.clone();
                    let picked = action_id.clone();
                    div()
                        .id(gpui::SharedString::from(format!(
                            "keymap-action-choice-{action_id}"
                        )))
                        .flex()
                        .items_center()
                        .h(px(24.0))
                        .px(px(8.0))
                        .text_size(px(11.0))
                        .text_color(Colors::text_secondary())
                        .cursor(gpui::CursorStyle::PointingHand)
                        .hover(|style| style.bg(Colors::state_hover()))
                        .child(label.clone())
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            let picked = picked.clone();
                            let label = label.clone();
                            let _ = entity.update(cx, |this, cx| {
                                // Start where the action already lives, so a new
                                // key for it lands beside the ones it has.
                                let scope = this
                                    .manager
                                    .rows()
                                    .iter()
                                    .find(|row| row.action_id == picked)
                                    .map(|row| row.scope)
                                    .unwrap_or_else(|| KeymapScope::home_of(&picked));
                                if let Some(dialog) = this.edit_dialog.as_mut() {
                                    dialog.action_id = picked.clone();
                                    dialog.action_label = label.clone();
                                    dialog.scope = scope;
                                }
                                // Action chosen: the keys belong to the recorder
                                // from here, which is the next thing the user
                                // will do.
                                this.arm_recorder(window, cx);
                            });
                        })
                })),
        )
}

fn edit_dialog_overlay(
    dialog: &EditDialogState,
    choices: Vec<(String, String)>,
    entity: gpui::Entity<KeymapWindow>,
) -> gpui::AnyElement {
    let on_save = {
        let entity = entity.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = entity.update(cx, |this, cx| this.save_edit_dialog(false, cx));
        }
    };
    let on_replace = {
        let entity = entity.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = entity.update(cx, |this, cx| this.save_edit_dialog(true, cx));
        }
    };
    let on_cancel = {
        let entity = entity.clone();
        move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = entity.update(cx, |this, cx| this.close_edit_dialog(cx));
        }
    };
    let on_arm = {
        let entity = entity.clone();
        move |_: &gpui::MouseDownEvent, window: &mut Window, cx: &mut App| {
            let _ = entity.update(cx, |this, cx| this.arm_recorder(window, cx));
        }
    };

    let conflict_lines: Vec<_> = dialog
        .conflicts
        .iter()
        .map(|c| {
            format!(
                "{} already uses {} in {}. Replace takes it off that action.",
                c.action,
                format_keystroke_list(std::slice::from_ref(&c.keystroke)),
                c.scope.label()
            )
        })
        .collect();
    let scope_choices = KeymapScope::ALL.into_iter().map(|scope| {
        let entity = entity.clone();
        scope_chip(
            gpui::SharedString::from(format!("keymap-edit-scope-{}", scope.key())),
            scope.label(),
            dialog.scope == scope,
            move |_, _, cx| {
                let _ = entity.update(cx, |this, cx| {
                    if let Some(dialog) = this.edit_dialog.as_mut() {
                        dialog.scope = scope;
                        // A conflict found in the old scope says nothing
                        // about the new one.
                        dialog.conflicts.clear();
                        dialog.show_conflict = false;
                    }
                    cx.notify();
                });
            },
        )
    });

    div()
        .absolute()
        .inset_0()
        .bg(Colors::with_alpha(Colors::surface_base(), 0.72))
        // Modal scrim. Without this the keymap table keeps its hitbox under the
        // dialog, so the wheel scrolls the rows behind the dialog.
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .w(px(420.0))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .p(px(14.0))
                .rounded(px(crate::theme::radius::SURFACE))
                .bg(Colors::surface_panel())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(if dialog.action_id.is_empty() {
                            "Create Keybinding".to_string()
                        } else {
                            format!("Edit {}", dialog.action_label)
                        }),
                )
                .child(fb_field_label("Action"))
                .child(if dialog.action_id.is_empty() {
                    action_picker(dialog, choices, entity.clone()).into_any_element()
                } else {
                    div()
                        .text_size(px(11.0))
                        .text_color(Colors::text_primary())
                        .child(dialog.action_id.clone())
                        .into_any_element()
                })
                .child(fb_field_label("Keystroke"))
                .child(
                    div()
                        .on_mouse_down(MouseButton::Left, on_arm)
                        .child(key_recorder_field(
                            &dialog.recorder,
                            if dialog.recorder.armed {
                                "Press the keys you want"
                            } else {
                                "Click to record again"
                            },
                            dialog.recorder.armed,
                        )),
                )
                .child(fb_field_label("Scope"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(4.0))
                        .children(scope_choices),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(Colors::text_muted())
                        .child(if dialog.scope == KeymapScope::Global {
                            "Works everywhere. Editors cannot reuse a global key."
                        } else {
                            "Works while this editor has the keyboard."
                        }),
                )
                .children(if dialog.show_conflict {
                    Some(
                        div()
                            .text_size(px(10.5))
                            .text_color(Colors::status_warning())
                            .children(conflict_lines.into_iter().map(|line| div().child(line))),
                    )
                } else {
                    None
                })
                .child({
                    let mut actions =
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(8.0))
                            .child(fb_button(
                                "keymap-edit-cancel",
                                "Cancel",
                                FbButtonKind::Default,
                                true,
                                on_cancel,
                            ));
                    if dialog.show_conflict {
                        actions = actions.child(fb_button(
                            "keymap-edit-replace",
                            "Replace Existing",
                            FbButtonKind::Primary,
                            true,
                            on_replace,
                        ));
                    } else {
                        actions = actions.child(fb_button(
                            "keymap-edit-save",
                            "Save",
                            FbButtonKind::Primary,
                            true,
                            on_save,
                        ));
                    }
                    actions
                }),
        )
        .into_any_element()
}

/// The Keymap window over a fresh Default profile, for the preview harness.
///
/// `changed` rebinds Undo so one row shows the Changed badge and its reset,
/// and selects it; `query` is typed into the search field.
#[cfg(feature = "ui-preview")]
pub fn open_preview_keymap_window(
    options: gpui::WindowOptions,
    changed: bool,
    query: &'static str,
    cx: &mut App,
) -> gpui::Result<WindowHandle<KeymapWindow>> {
    // An empty data folder of its own: no real overrides leak in.
    let dir = std::env::temp_dir().join("fb-keymap-preview");
    let _ = std::fs::remove_dir_all(&dir);
    let mut manager = KeymapManager::new(dir);
    let selected = changed.then(|| {
        let _ = manager.tap_binding(
            "edit:undo",
            vec!["ctrl+alt+z".into()],
            KeymapScope::Global,
            None,
            true,
        );
        manager
            .rows()
            .iter()
            .find(|row| row.is_user_override)
            .map(|row| row.id.clone())
    });
    let on_changed: KeymapChangedCb = Arc::new(|_, _| {});
    cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let mut view = KeymapWindow::new(manager, on_changed, cx);
            view.selected_row_id = selected.flatten();
            view.search_input.set_value(query);
            view.filter_query = query.to_string();
            view
        })
    })
}

pub fn open_keymap_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    manager: KeymapManager,
    on_changed: KeymapChangedCb,
    cx: &mut App,
) -> Result<WindowHandle<KeymapWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(KEYMAP_WINDOW_WIDTH), px(KEYMAP_WINDOW_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Transparent;
    options.window_min_size = Some(size(
        px(KEYMAP_WINDOW_MIN_WIDTH),
        px(KEYMAP_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(|cx| KeymapWindow::new(manager, on_changed, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table dims where an action lives and brightens what it is.
    #[test]
    fn an_action_label_splits_into_its_path_and_its_name() {
        assert_eq!(
            split_action_label("Audio › Tap Tempo › Tap"),
            ("Audio › Tap Tempo › ", "Tap")
        );
        assert_eq!(split_action_label("Edit › Undo"), ("Edit › ", "Undo"));
        assert_eq!(split_action_label("Undo"), ("", "Undo"));
    }

    /// One keycap per alternative key, in the order they were bound.
    #[test]
    fn each_alternative_key_is_its_own_keycap() {
        let keys = format_keystrokes(&["ctrl+shift+z".into(), "ctrl+y".into()]);
        assert_eq!(keys.len(), 2);
        assert!(format_keystrokes(&[]).is_empty());
    }
}

//! Keymap / Keyboard Shortcuts editor dialog.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, px, size, uniform_list, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{fb_button, fb_field_label, FbButtonKind};
use crate::components::form::select::{select, SelectOption};
use crate::components::key_recorder::{key_recorder_field, KeyRecorderState};
use crate::components::text_input::{
    bind_mouse_selection, text_field_with_callbacks, TextInputAction, TextInputState,
};
use crate::components::title_bar::external_window_titlebar;
use crate::keymap::{
    format_keystroke_list, KeymapConflict, KeymapManager, KeymapRow, PROFILE_DESCRIPTORS,
};
use crate::theme::{self, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const KEYMAP_WINDOW_WIDTH: f32 = 1200.0;
pub const KEYMAP_WINDOW_HEIGHT: f32 = 760.0;
pub const KEYMAP_WINDOW_MIN_WIDTH: f32 = 960.0;
pub const KEYMAP_WINDOW_MIN_HEIGHT: f32 = 620.0;

const ROW_H: f32 = 26.0;
const HEADER_H: f32 = 28.0;
const FOOTER_H: f32 = 24.0;
const TOPBAR_H: f32 = 36.0;

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
    context: String,
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
        context: String,
        captured: Option<String>,
    ) -> Self {
        Self {
            action_id,
            action_label,
            arguments_json,
            context,
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
            context: "Studio".into(),
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
}

impl KeymapWindow {
    fn new(manager: KeymapManager, on_changed: KeymapChangedCb, cx: &mut Context<Self>) -> Self {
        let json = manager.json_text().unwrap_or_default();
        Self {
            manager,
            on_changed,
            view: ViewMode::Table,
            search_input: TextInputState::new("keymap-search", cx.focus_handle())
                .with_placeholder("Filter action names..."),
            filter_query: String::new(),
            filter_pending: String::new(),
            json_input: TextInputState::new("keymap-json", cx.focus_handle())
                .with_accessible_label("Keymap JSON"),
            json_error: None,
            selected_row_id: None,
            profile_select_open: false,
            edit_dialog: None,
            create_dialog_open: false,
            status_message: None,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            recorder_intercept: None,
        }
        .with_json_text(json)
    }

    fn with_json_text(mut self, text: String) -> Self {
        self.json_input.set_value(text);
        self
    }

    fn visible_rows(&self) -> Vec<KeymapRow> {
        self.manager
            .filtered_rows(&self.filter_query)
            .into_iter()
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
            row.context.clone().unwrap_or_else(|| "Studio".to_string()),
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
            .tap_binding(&action_id, keys, Some(dialog.context.clone()), None, force)
        {
            Ok(conflicts) if !conflicts.is_empty() && !force => {
                let mut restored = dialog;
                restored.conflicts = conflicts;
                restored.show_conflict = true;
                self.edit_dialog = Some(restored);
            }
            Ok(_) => {
                let _ = self.manager.save_changes();
                self.json_input
                    .set_value(self.manager.json_text().unwrap_or_default());
                self.publish_changes(cx);
                self.status_message = Some("Keybinding saved.".into());
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
                                let _ = this.manager.save_changes();
                                this.json_input
                                    .set_value(this.manager.json_text().unwrap_or_default());
                                this.publish_changes(cx);
                                this.status_message = Some(format!("Imported {}", path.display()));
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
            (true, "e") => {
                self.view = ViewMode::Json;
                self.json_input
                    .set_value(self.manager.json_text().unwrap_or_default());
                cx.notify();
            }
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
                        if self
                            .manager
                            .rows()
                            .iter()
                            .any(|row| row.id == id && row.is_user_override)
                        {
                            self.manager.reset_binding(&id);
                            let _ = self.manager.save_changes();
                            self.publish_changes(cx);
                            cx.notify();
                        }
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

        let top_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(TOPBAR_H))
            .px(px(12.0))
            .border_b(px(1.0))
            .border_color(Colors::border_subtle())
            .child(div().w(px(320.0)).child(text_field_with_callbacks(
                &self.search_input,
                self.search_input.is_focused(window),
                search_callbacks,
            )))
            .child(div().flex_1())
            .child(fb_button(
                "keymap-edit-json",
                "Edit in JSON  Ctrl+E",
                FbButtonKind::Default,
                true,
                {
                    let entity = entity.clone();
                    move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            this.view = ViewMode::Json;
                            this.json_input
                                .set_value(this.manager.json_text().unwrap_or_default());
                            cx.notify();
                        });
                    }
                },
            ))
            .child(fb_button(
                "keymap-import",
                "Import",
                FbButtonKind::Default,
                true,
                {
                    let entity = entity.clone();
                    move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| this.import_profile(cx));
                    }
                },
            ))
            .child(fb_button(
                "keymap-export",
                "Export",
                FbButtonKind::Default,
                true,
                {
                    let entity = entity.clone();
                    move |_, _, cx| {
                        let _ = entity.update(cx, |this, cx| this.export_profile(cx));
                    }
                },
            ))
            .child(fb_button(
                "keymap-create",
                "Create Keybinding  Ctrl+K",
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
            ));

        let profile_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .h(px(30.0))
            .px(px(12.0))
            .child(fb_field_label("Profile"))
            .child(div().w(px(200.0)).child(select(
                "keymap-profile",
                Some(self.manager.active_profile_id()),
                "Profile",
                profile_options,
                self.profile_select_open,
                false,
                on_profile_toggle,
                on_profile_change,
            )));

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
                    keymap_row_element(row, selected).on_mouse_down(
                        MouseButton::Left,
                        move |event, window, cx| {
                            if event.click_count >= 2 {
                                let _ = entity.update(cx, |this, cx| {
                                    this.open_edit_for_row(&row_clone, window, cx);
                                });
                            } else {
                                let _ = entity.update(cx, |this, cx| {
                                    this.selected_row_id = Some(row_clone.id.clone());
                                    cx.notify();
                                });
                            }
                        },
                    )
                })
                .collect()
        })
        .size_full()
        .track_scroll(&self.scroll);

        let empty_state = if row_count == 0 {
            Some(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(120.0))
                    .text_color(Colors::text_muted())
                    .text_size(px(12.0))
                    .child("No keybindings found"),
            )
        } else {
            None
        };

        let table_view = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .child(table_body)
                    .child(keymap_scrollbar_thumb(scroll_for_thumb))
                    .children(empty_state),
            );

        let json_view = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .px(px(12.0))
            .py(px(8.0))
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(fb_button(
                        "keymap-json-back",
                        "Back to Table",
                        FbButtonKind::Default,
                        true,
                        {
                            let entity = entity.clone();
                            move |_, _, cx| {
                                let _ = entity.update(cx, |this, cx| {
                                    this.view = ViewMode::Table;
                                    cx.notify();
                                });
                            }
                        },
                    ))
                    .child(fb_button(
                        "keymap-json-apply",
                        "Apply JSON",
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
            .child(div().flex_1().min_h_0().child(text_field_with_callbacks(
                &self.json_input,
                self.json_input.is_focused(window),
                json_callbacks,
            )))
            .children(self.json_error.as_ref().map(|error| {
                div()
                    .text_color(Colors::status_error())
                    .text_size(px(11.0))
                    .child(error.clone())
            }));

        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(FOOTER_H))
            .px(px(12.0))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .text_size(px(10.0))
            .text_color(Colors::text_muted())
            .child(format!(
                "{} actions · {} visible · profile: {} · {} conflicts",
                self.manager.rows().len(),
                row_count,
                self.manager.active_profile_label(),
                self.manager.conflict_count()
            ))
            .children(self.status_message.as_ref().map(|msg| {
                div()
                    .ml(px(12.0))
                    .text_color(Colors::status_warning())
                    .child(msg.clone())
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
            .child(profile_row)
            .child(if self.view == ViewMode::Table {
                table_view.into_any_element()
            } else {
                json_view.into_any_element()
            })
            .child(footer)
            .children(edit_overlay)
    }
}

fn keymap_table_header() -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .h(px(HEADER_H))
        .bg(Colors::surface_panel())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_muted())
        .child(header_cell("Action", 360.0))
        .child(vsep())
        .child(header_cell("Arguments", 140.0))
        .child(vsep())
        .child(header_cell("Keystrokes", 220.0))
        .child(vsep())
        .child(header_cell("Context", 200.0))
        .child(vsep())
        .child(header_cell("Source", 120.0))
}

fn header_cell(label: &'static str, width: f32) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .w(px(width))
        .px(px(8.0))
        .child(label)
}

fn vsep() -> impl IntoElement {
    div().w(px(1.0)).h_full().bg(Colors::border_subtle())
}

fn keymap_row_element(row: &KeymapRow, selected: bool) -> gpui::Div {
    let alt = row.id.len() % 2 == 0;
    let text_color = if row.enabled {
        Colors::text_primary()
    } else {
        Colors::text_muted()
    };
    let keystrokes = format_keystroke_list(&row.keystrokes);
    let args = row
        .arguments_json
        .clone()
        .unwrap_or_else(|| "—".to_string());
    let context = row.context.clone().unwrap_or_else(|| "Studio".to_string());
    let source = if row.is_conflict {
        "Conflict"
    } else {
        row.source.label()
    };
    div()
        .flex()
        .flex_row()
        .h(px(ROW_H))
        .bg(if selected {
            Colors::accent_soft()
        } else if alt {
            Colors::surface_raised()
        } else {
            Colors::surface_base()
        })
        .text_color(text_color)
        .text_size(px(11.0))
        .cursor(gpui::CursorStyle::PointingHand)
        .child(body_cell(&row.action_label, 360.0))
        .child(vsep())
        .child(body_cell(&args, 140.0))
        .child(vsep())
        .child(
            body_cell(&keystrokes, 220.0).text_color(if row.keystrokes.is_empty() {
                Colors::text_muted()
            } else {
                Colors::text_secondary()
            }),
        )
        .child(vsep())
        .child(body_cell(&context, 200.0))
        .child(vsep())
        .child(body_cell(source, 120.0).text_color(if row.is_conflict {
            Colors::status_warning()
        } else {
            Colors::text_muted()
        }))
}

fn body_cell(label: &str, width: f32) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .w(px(width))
        .px(px(8.0))
        .overflow_hidden()
        .truncate()
        .child(label.to_string())
}

fn keymap_scrollbar_thumb(scroll: ScrollHandle) -> gpui::AnyElement {
    let viewport_h: f32 = scroll.bounds().size.height.into();
    let max_y: f32 = scroll.max_offset().y.into();
    let raw_y: f32 = scroll.offset().y.into();
    let offset_y: f32 = -raw_y;

    if viewport_h <= 0.0 || max_y <= 0.5 {
        return div().w(px(0.0)).h(px(0.0)).into_any_element();
    }

    let content_h = viewport_h + max_y;
    let min_thumb = 24.0_f32;
    let thumb_h = ((viewport_h / content_h) * viewport_h).max(min_thumb);
    let track_room = (viewport_h - thumb_h).max(0.0);
    let progress = (offset_y / max_y).clamp(0.0, 1.0);
    let thumb_top = progress * track_room;

    div()
        .absolute()
        .top(px(thumb_top))
        .right(px(2.0))
        .w(px(4.0))
        .h(px(thumb_h))
        .rounded(px(crate::theme::radius::PILL))
        .bg(Colors::with_alpha(Colors::text_primary(), 0.22))
        .into_any_element()
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
                                if let Some(dialog) = this.edit_dialog.as_mut() {
                                    dialog.action_id = picked.clone();
                                    dialog.action_label = label.clone();
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
        .map(|c| format!("{} ({})", c.action, c.keystroke))
        .collect();

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
                .child(fb_field_label("Context"))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(Colors::text_secondary())
                        .child(dialog.context.clone()),
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

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| KeymapWindow::new(manager, on_changed, cx))
    })
    .map_err(|error| error.to_string())
}

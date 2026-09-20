use gpui::{
    App, Bounds, Context, EntityInputHandler, FocusHandle, KeyDownEvent, Pixels, Point,
    UTF16Selection, Window,
};
use std::ops::Range;
use std::path::PathBuf;

use crate::components::context_menu::ContextMenuEntry;
use crate::components::plugin_picker::{
    compute_filter_result, ensure_default_highlight, move_highlight, page_size_for_height,
    sync_selection_from_highlight, visible_plugin_id_at, PluginPickerState,
};
use crate::components::text_input::{is_repeatable_edit_key, TextInputAction, TextInputState};
use crate::components::timeline::timeline_state::{
    is_project_routing_track, ClipType, TempoCurve, TrackTimebase, TrackType,
};
use crate::i18n::I18n;

use super::helpers::{is_supported_audio_ext, is_text_input_key};
use super::{
    ContextMenuTarget, ContextTarget, OpenPopover, RightDockTab, StudioLayout, TextMenuTarget,
};

/// Inspector inline name-edit fields — the focus-handle-backed track-name and
/// clip-name text inputs plus the ids they are currently bound to. `StudioLayout`
/// decomposition slice; built via `new(cx)` (the inputs need a focus handle).
pub(crate) struct InspectorNameEditState {
    /// Inspector track-name edit field.
    pub name_input: TextInputState,
    /// Track id the `name_input` is currently editing; `None` when none selected.
    pub name_bound: Option<String>,
    /// Inspector clip-name edit field.
    pub clip_name_input: TextInputState,
    /// Clip id the `clip_name_input` is currently editing; `None` when none.
    pub clip_name_bound: Option<String>,
    /// Host-owned state for the Inspector's track-colour picker (see
    /// [`crate::components::color_picker`]). Lives beside the name fields
    /// because it carries a text input with the same key-routing needs.
    pub color_picker: crate::components::ColorPickerState,
    /// Track id the open picker is editing, so a selection change while the
    /// popover is open cannot recolour the wrong track.
    pub color_bound: Option<String>,
}

impl InspectorNameEditState {
    pub(super) fn new(cx: &mut Context<StudioLayout>) -> Self {
        Self {
            name_input: TextInputState::new("inspector-name-input", cx.focus_handle())
                .with_placeholder("Track name")
                .blur_on_outside_click(true),
            name_bound: None,
            clip_name_input: TextInputState::new("inspector-clip-name-input", cx.focus_handle())
                .with_placeholder("Clip name")
                .blur_on_outside_click(true),
            clip_name_bound: None,
            color_picker: crate::components::ColorPickerState::new(
                "inspector-color-hex",
                cx.focus_handle(),
                crate::components::ColorPickerValue::custom(crate::color::auto_color_for_index(0)),
                crate::color::auto_color_for_index(0),
                crate::color::load_recent_colors(),
            ),
            color_bound: None,
        }
    }
}

fn menu_item_enabled(
    label: impl Into<String>,
    command: impl Into<String>,
    enabled: bool,
) -> ContextMenuEntry {
    let label = label.into();
    let command = command.into();
    if enabled {
        ContextMenuEntry::item(label, command)
    } else {
        ContextMenuEntry::disabled_item(label, command)
    }
}

fn danger_menu_item_enabled(
    label: impl Into<String>,
    command: impl Into<String>,
    enabled: bool,
) -> ContextMenuEntry {
    let label = label.into();
    let command = command.into();
    if enabled {
        ContextMenuEntry::danger_item(label, command)
    } else {
        ContextMenuEntry::disabled_item(label, command)
    }
}

impl StudioLayout {
    pub(super) fn command_palette_visible_count(&self) -> usize {
        crate::components::command_palette_entries(&self.command_palette.query).len()
    }

    pub(super) fn handle_command_palette_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.command_palette.is_open {
            return false;
        }
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                self.command_palette.close();
                self.overlay.text_context_menu = None;
                self.focus_handle.focus(window, cx);
                true
            }
            "arrow_down" | "arrowdown" | "down" => {
                let max = self.command_palette_visible_count().saturating_sub(1);
                self.command_palette.selected_index =
                    (self.command_palette.selected_index + 1).min(max);
                true
            }
            "arrow_up" | "arrowup" | "up" => {
                self.command_palette.selected_index =
                    self.command_palette.selected_index.saturating_sub(1);
                true
            }
            "enter" | "numpad_enter" => {
                if let Some(entry) =
                    crate::components::command_palette_entries(&self.command_palette.query)
                        .get(self.command_palette.selected_index)
                        .cloned()
                {
                    self.command_palette.close();
                    self.focus_handle.focus(window, cx);
                    self.dispatch_command_id_from_bounds(&entry.command, Some(window.bounds()), cx);
                }
                true
            }
            _ => {
                let focused = self.command_palette_input.is_focused(window);
                if focused || is_text_input_key(event) {
                    let action = self.command_palette_input.handle_key_ime(event, Some(cx));
                    self.sync_text_input_target(TextMenuTarget::CommandPalette);
                    return !matches!(action, TextInputAction::Pass);
                }
                false
            }
        }
    }

    pub(super) fn project_switcher_visible_entries(
        &self,
    ) -> Vec<crate::components::project_switcher::ProjectSummary> {
        let query = self.project_switcher.query.trim().to_lowercase();
        self.project_switcher
            .recent_projects
            .iter()
            .filter(|project| !project.is_current)
            .filter(|project| {
                if query.is_empty() {
                    return true;
                }
                let path = project
                    .path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                project.name.to_lowercase().contains(&query) || path.contains(&query)
            })
            .cloned()
            .collect()
    }

    pub(super) fn project_switcher_visible_recent_paths(&self) -> Vec<PathBuf> {
        self.project_switcher_visible_entries()
            .into_iter()
            .filter_map(|project| project.path)
            .collect()
    }

    pub(super) fn project_switcher_visible_count(&self) -> usize {
        1 + self.project_switcher_visible_recent_paths().len()
    }

    pub(super) fn handle_project_switcher_key(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.project_switcher.is_open {
            return false;
        }
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }
        let key = event.keystroke.key.as_str();
        if self.overlay.text_context_menu.take().is_some() && key == "escape" {
            cx.notify();
            return true;
        }

        let search_focused = self.project_switcher_search_input.is_focused(window);
        match key {
            "escape" => {
                self.project_switcher.is_open = false;
                self.overlay.text_context_menu = None;
                true
            }
            "arrow_down" | "arrowdown" | "down" => {
                let max = self.project_switcher_visible_count().saturating_sub(1);
                self.project_switcher.selected_index =
                    (self.project_switcher.selected_index + 1).min(max);
                true
            }
            "arrow_up" | "arrowup" | "up" => {
                self.project_switcher.selected_index =
                    self.project_switcher.selected_index.saturating_sub(1);
                true
            }
            "enter" | "numpad_enter" => {
                let idx = self.project_switcher.selected_index;
                let owner_bounds = Some(window.bounds());
                if idx == 0 {
                    self.handle_project_switch_current_row(cx);
                } else if let Some(project) = self
                    .project_switcher_visible_entries()
                    .get(idx.saturating_sub(1))
                {
                    if let Some(path) = project.path.clone() {
                        self.request_switch_project(
                            crate::layout::project_switch::ProjectSwitchRequest {
                                target_path: path,
                                target_name: Some(project.name.clone()),
                                source:
                                    crate::layout::project_switch::ProjectSwitchSource::ProjectSwitcher,
                            },
                            owner_bounds,
                            cx,
                        );
                    }
                }
                true
            }
            _ => {
                if search_focused || is_text_input_key(event) {
                    let action = self
                        .project_switcher_search_input
                        .handle_key_ime(event, Some(cx));
                    self.sync_text_input_target(TextMenuTarget::ProjectSwitcherSearch);
                    return !matches!(action, TextInputAction::Pass);
                }
                false
            }
        }
    }

    /// Routes keys to the inline BPM numeric editor while it is open. Enter
    /// commits, Escape cancels, everything else edits the field. Runs first in
    /// the key chain so it captures input without a GPUI focus grab.
    pub(super) fn handle_bpm_edit_key(
        &mut self,
        event: &KeyDownEvent,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.tempo_edit.bpm_editing {
            return false;
        }
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }
        // Arrow up/down increment/decrement the numeric value (Shift = coarse
        // ×10). Left/right stay as caret movement inside the field.
        let key = event.keystroke.key.as_str();
        if matches!(key, "up" | "arrow_up" | "down" | "arrow_down") {
            let sign = if matches!(key, "up" | "arrow_up") {
                1.0
            } else {
                -1.0
            };
            let coarse = if event.keystroke.modifiers.shift {
                10.0
            } else {
                1.0
            };
            self.nudge_bpm_edit(sign * coarse, cx);
            return true;
        }
        let action = self
            .tempo_edit
            .bpm_input
            .handle_key_with_clipboard(event, Some(cx));
        match action {
            TextInputAction::Submit => self.commit_bpm_edit(cx),
            TextInputAction::Cancel => self.cancel_bpm_edit(cx),
            TextInputAction::Consumed | TextInputAction::Pass => {}
        }
        cx.notify();
        true
    }

    pub(super) fn handle_ts_edit_key(
        &mut self,
        event: &KeyDownEvent,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.tempo_edit.ts_editing {
            return false;
        }
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }
        if event.keystroke.key == "escape" {
            self.cancel_ts_edit(cx);
            return true;
        }
        if event.keystroke.key == "tab" {
            self.tempo_edit.ts_edit_focus_num = !self.tempo_edit.ts_edit_focus_num;
            if self.tempo_edit.ts_edit_focus_num {
                self.tempo_edit.ts_num_input.select_all();
            } else {
                self.tempo_edit.ts_den_input.select_all();
            }
            cx.notify();
            return true;
        }
        let active = if self.tempo_edit.ts_edit_focus_num {
            &mut self.tempo_edit.ts_num_input
        } else {
            &mut self.tempo_edit.ts_den_input
        };
        let action = active.handle_key_with_clipboard(event, Some(cx));
        match action {
            TextInputAction::Submit => self.commit_ts_edit(cx),
            TextInputAction::Cancel => self.cancel_ts_edit(cx),
            TextInputAction::Consumed | TextInputAction::Pass => {}
        }
        cx.notify();
        true
    }

    pub(super) fn handle_browser_key(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.panels.browser {
            return false;
        }
        let search_focused = self.browser_search_input.is_focused(window);
        // The tree now owns a real focus handle, so arrows, Enter and
        // type-ahead survive a mouse click on a row. Before this the whole
        // handler was gated on the *search field*, which meant clicking a row
        // killed the keyboard until the user clicked the search box again.
        let tree_focused = self.browser_sidebar.read(cx).tree_focus.is_focused(window);
        if !search_focused && !tree_focused {
            return false;
        }
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }
        let key = event.keystroke.key.as_str();
        if self.overlay.text_context_menu.take().is_some() && key == "escape" {
            cx.notify();
            return true;
        }

        match key {
            "arrow_down" | "down" => {
                let _ = self.file_browser.select_next();
                self.reapply_browser_selection(cx);
                cx.notify();
                true
            }
            "arrow_up" | "up" => {
                let _ = self.file_browser.select_previous();
                self.reapply_browser_selection(cx);
                cx.notify();
                true
            }
            "arrow_left" | "left" => {
                if self.file_browser.collapse_selected_or_parent() {
                    self.drain_browser_directory_loads(cx);
                    self.reapply_browser_selection(cx);
                }
                cx.notify();
                true
            }
            "arrow_right" | "right" => {
                if self.file_browser.expand_selected() {
                    self.drain_browser_directory_loads(cx);
                    self.reapply_browser_selection(cx);
                }
                cx.notify();
                true
            }
            // Escape belongs to the tree only; in the search field it still
            // cancels the edit through the text-input tail below.
            "escape" if tree_focused && !search_focused => {
                self.file_browser.clear_type_ahead();
                self.stop_browser_audition();
                cx.notify();
                true
            }
            "enter" | "numpad_enter" => {
                if let Some(selected_path) = self.file_browser.selected.clone() {
                    // Directory-ness comes from the cached node. The old
                    // `selected_path.is_dir()` was a blocking `stat` on the UI
                    // thread inside a key handler.
                    if self.file_browser.selected_is_expandable() {
                        let id = selected_path.to_string_lossy().to_string();
                        let expanded = self.file_browser.toggle_node(&id, Some(&selected_path));
                        if expanded {
                            self.drain_browser_directory_loads(cx);
                        }
                    } else {
                        let ext = selected_path
                            .extension()
                            .and_then(|s| s.to_str())
                            .map(|s| s.to_ascii_lowercase())
                            .unwrap_or_default();
                        if is_supported_audio_ext(&ext) {
                            let timeline = self.timeline.clone();
                            let layout = cx.entity().clone();
                            let path = selected_path.clone();
                            let path_for_decode = path.clone();
                            let timeline_for_decode = timeline.clone();
                            timeline.update(cx, |t, cx| {
                                let path_key = path.to_string_lossy().to_string();
                                let name = path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| "Imported Audio".to_string());
                                t.state
                                    .import_audio_to_selected_or_new_track(path_key, name);
                                cx.notify();
                            });
                            let _ = layout.update(cx, |this, cx| {
                                this.mark_dirty();
                                this.mark_engine_media_dirty();
                                this.schedule_audio_project_sync(cx, false, "browser_import");
                            });
                            let path_key = path_for_decode.to_string_lossy().to_string();
                            let _ = layout.update(cx, move |this, cx| {
                                this.spawn_timeline_audio_import_jobs(
                                    cx,
                                    timeline_for_decode,
                                    path_for_decode,
                                    path_key,
                                );
                            });
                        }
                    }
                }
                true
            }
            _ => {
                if search_focused {
                    let action = self.browser_search_input.handle_key_ime(event, Some(cx));
                    self.sync_text_input_target(TextMenuTarget::BrowserSearch);
                    return !matches!(action, TextInputAction::Pass);
                }
                // Type-ahead. Gated on the tree's own focus handle — and it
                // must consume the key, because further down the capture chain
                // the virtual keyboard turns bare letters into MIDI notes.
                let modifiers = event.keystroke.modifiers;
                if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
                    return false;
                }
                let mut chars = event.keystroke.key.chars();
                let Some(ch) = chars.next() else {
                    return false;
                };
                if chars.next().is_some() || !ch.is_alphanumeric() {
                    return false;
                }
                if self
                    .file_browser
                    .type_ahead(ch, std::time::Instant::now())
                    .is_some()
                {
                    self.reapply_browser_selection(cx);
                }
                cx.notify();
                true
            }
        }
    }

    /// Push the model's current selection back through the shared selection
    /// operation, so a keyboard move decodes peaks, auditions and scrolls into
    /// view exactly like a mouse click does.
    fn reapply_browser_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.file_browser.selected.clone() {
            self.apply_browser_selection(path, cx);
        }
    }

    pub(super) fn handle_settings_dialog_key(
        &mut self,
        _event: &KeyDownEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> bool {
        // Settings is now an external window that handles its own keyboard events.
        false
    }

    pub(super) fn handle_add_track_dialog_key(
        &mut self,
        _event: &KeyDownEvent,
        _window: &Window,
        _cx: &mut Context<Self>,
    ) -> bool {
        // Add Track is now an external window that handles its own keyboard events.
        false
    }

    pub(super) fn handle_plugin_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.plugin_picker.is_open {
            return false;
        }
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }

        let modifiers = &event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();

        if key == "escape" {
            self.plugin_picker = PluginPickerState::closed();
            cx.notify();
            return true;
        }

        if (modifiers.control || modifiers.platform) && key.eq_ignore_ascii_case("f") {
            self.plugin_picker_search_input
                .focus_handle
                .focus(window, cx);
            cx.notify();
            return true;
        }

        let Some(index) = self.plugin_search_index.clone() else {
            return self.handle_plugin_picker_text_input(event, window, cx);
        };

        match key {
            "enter" => {
                if let Some(id) =
                    visible_plugin_id_at(&self.plugin_picker, &index, &self.plugin_picker_prefs)
                {
                    if let Some((track_id, insert_index, insert_id)) =
                        self.apply_picked_insert(&id, cx)
                    {
                        self.open_insert_editor(&track_id, insert_index, &insert_id, window, cx);
                    }
                }
                true
            }
            // Navigation keys are the only ones that need the visible row count,
            // so the filter pass is computed lazily here — typing a character
            // (the `_` arm) never pays for it.
            "up" | "arrowup" | "down" | "arrowdown" | "home" | "end" | "pageup" | "pagedown" => {
                let visible_len = compute_filter_result(
                    &index,
                    &self.plugin_picker.query,
                    &self.plugin_picker.filters,
                    &self.plugin_picker_prefs,
                    std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some(),
                )
                .indices
                .len();
                let page = page_size_for_height(self.plugin_picker_prefs.window_height) as isize;
                match key {
                    "up" | "arrowup" => move_highlight(&mut self.plugin_picker, -1, visible_len),
                    "down" | "arrowdown" => move_highlight(&mut self.plugin_picker, 1, visible_len),
                    "home" => self.plugin_picker.highlighted_index = 0,
                    "end" => {
                        if visible_len > 0 {
                            self.plugin_picker.highlighted_index = visible_len - 1;
                        }
                    }
                    "pageup" => move_highlight(&mut self.plugin_picker, -page, visible_len),
                    "pagedown" => move_highlight(&mut self.plugin_picker, page, visible_len),
                    _ => unreachable!(),
                }
                sync_selection_from_highlight(
                    &mut self.plugin_picker,
                    &index,
                    &self.plugin_picker_prefs,
                );
                cx.notify();
                true
            }
            _ => self.handle_plugin_picker_text_input(event, window, cx),
        }
    }

    pub(super) fn handle_plugin_picker_text_input(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.plugin_picker_search_input.is_focused(window) || is_text_input_key(event) {
            // NOTE: stays on the non-IME path because this field is *also* hosted
            // in the separate InsertPickerWindow (a snapshot-mirrored entity that
            // can't share the StudioLayout IME bridge). `handle_key_with_clipboard`
            // inserts from key_char (Thai/accents work); it is deliberately excluded
            // from `focused_text_target` so the inline overlay is never bridged
            // either — keeping both hosts consistent and double-free.
            let action = self
                .plugin_picker_search_input
                .handle_key_with_clipboard(event, Some(cx));
            // `sync_text_input_target` already updates the query AND refreshes the
            // default highlight for this field — no second filter pass here.
            self.sync_text_input_target(TextMenuTarget::PluginPickerSearch);
            return !matches!(action, TextInputAction::Pass);
        }
        false
    }

    /// Route keys into the Add Automation (parameter) picker's search field while
    /// the overlay is open, so typing filters parameters instead of falling
    /// through to global DAW shortcuts (Space=play, Backspace=delete clip, …).
    /// Mirrors `handle_plugin_picker_text_input`; the overlay is an in-window
    /// popover (`OpenPopover::AutomationTargetPicker`), not a separate window.
    pub(super) fn handle_automation_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !matches!(
            self.overlay.open_popover,
            Some(OpenPopover::AutomationTargetPicker { .. })
        ) {
            return false;
        }
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }
        if event.keystroke.key.as_str() == "escape" {
            self.overlay.open_popover = None;
            cx.notify();
            return true;
        }
        // Feed every editing key (incl. Space/Backspace/arrows) into the field and
        // CONSUME it: while the overlay owns the keyboard, nothing should reach the
        // global transport/edit shortcuts (Space=play, Backspace=delete clip, …).
        if self.automation_picker_search_input.is_focused(window) || is_text_input_key(event) {
            let _ = self
                .automation_picker_search_input
                .handle_key_with_clipboard(event, Some(cx));
            self.sync_text_input_target(TextMenuTarget::AutomationPickerSearch);
            cx.notify();
            return true;
        }
        false
    }

    pub(super) fn text_input_mut(&mut self, target: TextMenuTarget) -> &mut TextInputState {
        match target {
            TextMenuTarget::CommandPalette => &mut self.command_palette_input,
            TextMenuTarget::ProjectSwitcherSearch => &mut self.project_switcher_search_input,
            TextMenuTarget::BrowserSearch => &mut self.browser_search_input,
            TextMenuTarget::PluginPickerSearch => &mut self.plugin_picker_search_input,
            TextMenuTarget::AutomationPickerSearch => &mut self.automation_picker_search_input,
            TextMenuTarget::InspectorName => &mut self.inspector_name_edit.name_input,
            TextMenuTarget::InspectorClipName => &mut self.inspector_name_edit.clip_name_input,
            TextMenuTarget::InspectorColorHex => {
                &mut self.inspector_name_edit.color_picker.hex_input
            }
            TextMenuTarget::TransportBpm => &mut self.tempo_edit.bpm_input,
            TextMenuTarget::TransportTimeSigNum => &mut self.tempo_edit.ts_num_input,
            TextMenuTarget::TransportTimeSigDen => &mut self.tempo_edit.ts_den_input,
        }
    }

    pub(super) fn text_input(&self, target: TextMenuTarget) -> &TextInputState {
        match target {
            TextMenuTarget::CommandPalette => &self.command_palette_input,
            TextMenuTarget::ProjectSwitcherSearch => &self.project_switcher_search_input,
            TextMenuTarget::BrowserSearch => &self.browser_search_input,
            TextMenuTarget::PluginPickerSearch => &self.plugin_picker_search_input,
            TextMenuTarget::AutomationPickerSearch => &self.automation_picker_search_input,
            TextMenuTarget::InspectorName => &self.inspector_name_edit.name_input,
            TextMenuTarget::InspectorClipName => &self.inspector_name_edit.clip_name_input,
            TextMenuTarget::InspectorColorHex => &self.inspector_name_edit.color_picker.hex_input,
            TextMenuTarget::TransportBpm => &self.tempo_edit.bpm_input,
            TextMenuTarget::TransportTimeSigNum => &self.tempo_edit.ts_num_input,
            TextMenuTarget::TransportTimeSigDen => &self.tempo_edit.ts_den_input,
        }
    }

    pub(super) fn sync_text_input_target(&mut self, target: TextMenuTarget) {
        match target {
            TextMenuTarget::CommandPalette => {
                self.command_palette.query = self.command_palette_input.value.clone();
                self.command_palette.selected_index = 0;
            }
            TextMenuTarget::ProjectSwitcherSearch => {
                self.project_switcher.query = self.project_switcher_search_input.value.clone();
                self.project_switcher.selected_index = 0;
            }
            TextMenuTarget::BrowserSearch => {
                self.file_browser
                    .set_filter(&self.browser_search_input.value);
            }
            TextMenuTarget::InspectorName => {
                // No-op here: committing the name to the bound track requires a
                // `Context` (the timeline is an entity), so the live commit lives
                // in `handle_inspector_key` / `commit_inspector_name`, which have
                // `cx`. This keeps `sync_text_input_target` `cx`-free like the
                // other arms.
            }
            TextMenuTarget::InspectorClipName => {
                // Same as InspectorName: clip rename commits need `Context`.
            }
            TextMenuTarget::InspectorColorHex => {
                // The colour picker owns the value and observes the input
                // state through its callback; the context menu only edits the
                // shared TextInputState here.
            }
            TextMenuTarget::PluginPickerSearch => {
                self.plugin_picker.query = self.plugin_picker_search_input.value.clone();
                if let Some(index) = self.plugin_search_index.as_ref() {
                    self.plugin_picker.highlighted_index = 0;
                    ensure_default_highlight(
                        &mut self.plugin_picker,
                        index,
                        &self.plugin_picker_prefs,
                    );
                }
            }
            TextMenuTarget::AutomationPickerSearch => {
                self.automation_picker_query = self.automation_picker_search_input.value.clone();
            }
            // The transport's inline editors commit on Enter or on blur, not on
            // every keystroke — so a clipboard edit deliberately leaves the
            // draft text alone here. Committing on paste would make the context
            // menu behave differently from typing and could apply a
            // half-finished tempo to the engine.
            TextMenuTarget::TransportBpm
            | TextMenuTarget::TransportTimeSigNum
            | TextMenuTarget::TransportTimeSigDen => {}
        }
    }

    pub(super) fn text_input_has_focus(&self, window: &Window) -> bool {
        self.command_palette_input.is_focused(window)
            || self.project_switcher_search_input.is_focused(window)
            || (self.panels.browser && self.browser_search_input.is_focused(window))
            || self.plugin_picker_search_input.is_focused(window)
            || self.automation_picker_search_input.is_focused(window)
            || (self.panels.inspector
                && self.right_dock_tab == RightDockTab::Inspector
                && (self.inspector_name_edit.name_input.is_focused(window)
                    || self.inspector_name_edit.clip_name_input.is_focused(window)
                    || self.inspector_color_hex_focused(window)))
    }

    /// The Inspector colour picker's hex field only counts as a live text
    /// target while its popover is actually open — a closed popover leaves the
    /// focus handle behind, and treating that as text focus would swallow
    /// transport keys forever.
    pub(super) fn inspector_color_hex_focused(&self, window: &Window) -> bool {
        self.inspector_name_edit.color_picker.open
            && self
                .inspector_name_edit
                .color_picker
                .hex_input
                .is_focused(window)
    }

    /// Route a key to the Inspector's track-name field when it owns focus.
    /// Returns `true` if consumed. Mirrors `handle_browser_key`'s text tail:
    /// the field edits in place and the new value is live-committed to the
    /// bound track via [`commit_inspector_name`] so TrackHeader/Mixer follow.
    pub(super) fn handle_inspector_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !(self.panels.inspector && self.right_dock_tab == RightDockTab::Inspector) {
            return false;
        }
        if self.inspector_color_hex_focused(window) {
            return self.handle_inspector_color_hex_key(event, window, cx);
        }
        let track_name_focused = self.inspector_name_edit.name_input.is_focused(window);
        let clip_name_focused = self.inspector_name_edit.clip_name_input.is_focused(window);
        if !track_name_focused && !clip_name_focused {
            return false;
        }
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }
        let action = if clip_name_focused {
            self.inspector_name_edit
                .clip_name_input
                .handle_key_ime(event, Some(cx))
        } else {
            self.inspector_name_edit
                .name_input
                .handle_key_ime(event, Some(cx))
        };
        match action {
            TextInputAction::Pass => false,
            TextInputAction::Cancel => {
                // Esc: drop focus back to the studio surface; reload happens on
                // next render because the bound name is unchanged.
                self.focus_handle.focus(window, cx);
                cx.notify();
                true
            }
            _ => {
                if clip_name_focused {
                    self.commit_inspector_clip_name(cx);
                } else {
                    self.commit_inspector_name(cx);
                }
                if matches!(action, TextInputAction::Submit) {
                    self.focus_handle.focus(window, cx);
                }
                cx.notify();
                true
            }
        }
    }

    /// Commit the Inspector name field's current value to the bound track.
    /// Marks the project dirty only on a real change (rename_track returns
    /// whether the stored name changed). Never marks the engine dirty — a name
    /// is project metadata only.
    pub(super) fn commit_inspector_name(&mut self, cx: &mut Context<Self>) {
        let Some(track_id) = self.inspector_name_edit.name_bound.clone() else {
            return;
        };
        let new_name = self.inspector_name_edit.name_input.value.clone();
        let changed = self.timeline.update(cx, |t, cx| {
            let changed = t.state.rename_track(&track_id, &new_name);
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            crate::components::inspector_debug(&format!(
                "edit track name track={track_id} new={new_name}"
            ));
            self.mark_dirty();
            self.push_mixer_snapshot_to_window(cx);
        }
    }

    pub(super) fn commit_inspector_clip_name(&mut self, cx: &mut Context<Self>) {
        let Some(clip_id) = self.inspector_name_edit.clip_name_bound.clone() else {
            return;
        };
        let new_name = self.inspector_name_edit.clip_name_input.value.clone();
        let changed = self.timeline.update(cx, |t, cx| {
            let changed = t.state.rename_clip(&clip_id, &new_name);
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            crate::components::inspector_debug(&format!(
                "edit clip name clip={clip_id} new={new_name}"
            ));
            self.mark_dirty();
            self.mark_engine_media_dirty();
            self.schedule_audio_project_sync(cx, false, "inspector_clip_name");
        }
    }

    /// Route a key to the Inspector colour picker's hex field. Every accepted
    /// keystroke re-parses the field for a live preview and pushes the parsed
    /// colour straight to the bound track, so the header/mixer follow while the
    /// user types. Enter commits (records the colour as recent and closes),
    /// Escape closes without further change — the previewed colour has already
    /// been applied, exactly like dragging the hue strip.
    fn handle_inspector_color_hex_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.is_held && !is_repeatable_edit_key(event) {
            return true;
        }
        let action = self
            .inspector_name_edit
            .color_picker
            .hex_input
            .handle_key_with_clipboard(event, Some(cx));
        match action {
            TextInputAction::Pass => false,
            TextInputAction::Cancel => {
                self.close_inspector_color_picker(window, cx);
                true
            }
            TextInputAction::Submit => {
                if let Some(color) = self.inspector_name_edit.color_picker.commit_hex() {
                    self.apply_inspector_color(color, cx);
                    self.inspector_name_edit.color_picker.remember_current();
                    self.close_inspector_color_picker(window, cx);
                } else {
                    cx.notify();
                }
                true
            }
            TextInputAction::Consumed => {
                self.inspector_name_edit.color_picker.on_hex_changed();
                if self.inspector_name_edit.color_picker.hex_error.is_none() {
                    let color = self.inspector_name_edit.color_picker.draft;
                    self.apply_inspector_color(color, cx);
                }
                cx.notify();
                true
            }
        }
    }

    /// Open the Inspector colour picker for `track_id`, seeded from that
    /// track's current colour.
    pub(crate) fn open_inspector_color_picker(&mut self, track_id: &str, cx: &mut Context<Self>) {
        let current = self
            .timeline
            .read(cx)
            .state
            .find_track(track_id)
            .map(|track| track.color);
        let fallback = crate::color::auto_color_for_index(0);
        self.inspector_name_edit.color_picker.reset(
            crate::components::ColorPickerValue::custom(current.unwrap_or(fallback)),
            fallback,
        );
        self.inspector_name_edit.color_bound = Some(track_id.to_string());
        self.inspector_name_edit.color_picker.open();
        cx.notify();
    }

    /// Close the picker, returning focus to the studio surface so transport
    /// keys work again immediately.
    pub(crate) fn close_inspector_color_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.inspector_name_edit.color_picker.open {
            return;
        }
        self.inspector_name_edit.color_picker.close();
        self.inspector_name_edit.color_bound = None;
        if self
            .inspector_name_edit
            .color_picker
            .hex_input
            .is_focused(window)
        {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    /// Commit a colour to the track the open picker is bound to, extending the
    /// change across a multi-track selection the same way the preset swatches
    /// do (see `build_inspector_callbacks`'s `on_set_color`).
    pub(crate) fn apply_inspector_color(&mut self, color: gpui::Rgba, cx: &mut Context<Self>) {
        let Some(track_id) = self.inspector_name_edit.color_bound.clone() else {
            return;
        };
        let changed = self.timeline.update(cx, |t, cx| {
            let mut changed = t.state.set_track_color(&track_id, color);
            if t.state.is_track_selected(&track_id)
                && t.state.selection.selected_track_ids.len() > 1
            {
                for other in t.state.selection.selected_track_ids.clone() {
                    if other == track_id {
                        continue;
                    }
                    changed |= t.state.set_track_color(&other, color);
                }
            }
            if changed {
                cx.notify();
            }
            changed
        });
        if changed {
            crate::components::inspector_debug(&format!(
                "edit track color track={track_id} via picker"
            ));
            self.mark_dirty();
            self.push_mixer_snapshot_to_window(cx);
        }
    }

    /// Whether a *live* main-window text field currently owns the keyboard —
    /// i.e. its focus handle is focused AND its overlay is actually open.
    ///
    /// This differs from [`text_input_has_focus`] in that it does NOT trust a
    /// focused search handle whose overlay has closed: GPUI keeps a closed
    /// overlay's `FocusHandle` "focused" (the handle is still ref-counted) even
    /// though its element is no longer rendered. That orphaned focus is exactly
    /// what silently killed every keyboard shortcut — see `reclaim` in render.
    ///
    /// Browser/inspector fields are gated on panel visibility for the same
    /// reason: closing the side panel leaves the search/name FocusHandle
    /// orphaned, which on Linux was enough to block Space/transport forever.
    pub(super) fn keyboard_text_capture_live(&self, window: &Window) -> bool {
        (self.command_palette.is_open && self.command_palette_input.is_focused(window))
            || (self.project_switcher.is_open
                && self.project_switcher_search_input.is_focused(window))
            || (self.plugin_picker.is_open && self.plugin_picker_search_input.is_focused(window))
            || (matches!(
                self.overlay.open_popover,
                Some(OpenPopover::AutomationTargetPicker { .. })
            ) && self.automation_picker_search_input.is_focused(window))
            || (self.panels.browser && self.browser_search_input.is_focused(window))
            || (self.panels.inspector
                && self.right_dock_tab == RightDockTab::Inspector
                && (self.inspector_name_edit.name_input.is_focused(window)
                    || self.inspector_name_edit.clip_name_input.is_focused(window)
                    || self.inspector_color_hex_focused(window)))
    }

    pub(super) fn is_text_editing_context(&self, window: &Window) -> bool {
        self.keyboard_text_capture_live(window)
    }

    pub(super) fn focused_widget_kind(&self, window: &Window) -> String {
        if self.command_palette.is_open && self.command_palette_input.is_focused(window) {
            "command_palette".to_string()
        } else if self.project_switcher.is_open
            && self.project_switcher_search_input.is_focused(window)
        {
            "project_switcher_search".to_string()
        } else if self.plugin_picker.is_open && self.plugin_picker_search_input.is_focused(window) {
            "add_insert_search".to_string()
        } else if matches!(
            self.overlay.open_popover,
            Some(OpenPopover::AutomationTargetPicker { .. })
        ) && self.automation_picker_search_input.is_focused(window)
        {
            "automation_picker_search".to_string()
        } else if self.panels.browser && self.browser_search_input.is_focused(window) {
            "browser_search".to_string()
        } else if self.panels.inspector
            && self.right_dock_tab == RightDockTab::Inspector
            && self.inspector_name_edit.name_input.is_focused(window)
        {
            "inspector_track_name".to_string()
        } else if self.panels.inspector
            && self.right_dock_tab == RightDockTab::Inspector
            && self.inspector_name_edit.clip_name_input.is_focused(window)
        {
            "inspector_clip_name".to_string()
        } else if self.focus_handle.is_focused(window) {
            "studio_shortcut_anchor".to_string()
        } else {
            "unknown".to_string()
        }
    }

    pub(super) fn context_entries(
        &self,
        target: &ContextTarget,
        cx: &mut Context<Self>,
    ) -> Vec<ContextMenuEntry> {
        // Backfill each dispatching item's keyboard shortcut from the active
        // keymap so context menus stay in sync with the profile without every
        // arm hardcoding accelerators. Explicit `with_shortcut` calls still win.
        let entries = self.context_entries_raw(target, cx);
        let manager = &self.keymap_manager;
        crate::components::context_menu::backfill_shortcuts(entries, |command| {
            manager.shortcut_for_command(command)
        })
    }

    fn context_entries_raw(
        &self,
        target: &ContextTarget,
        cx: &mut Context<Self>,
    ) -> Vec<ContextMenuEntry> {
        let i18n = I18n::new(&self.settings.read(cx).current.general.language);
        match target {
            ContextTarget::TimelineEmpty => vec![
                ContextMenuEntry::item(i18n.tr("context.timeline.add-audio"), "track:add-audio"),
                ContextMenuEntry::item(i18n.tr("context.timeline.add-midi"), "track:add-midi"),
                ContextMenuEntry::item(i18n.tr("menu.project-add_bus_track"), "track:add-bus"),
                ContextMenuEntry::Separator,
                menu_item_enabled(
                    i18n.tr("context.paste"),
                    "edit:paste",
                    !self.clip_clipboard.is_empty(),
                ),
                ContextMenuEntry::Separator,
                ContextMenuEntry::item(i18n.tr("context.zoom-in"), "view:zoom-in"),
                ContextMenuEntry::item(i18n.tr("context.zoom-out"), "view:zoom-out"),
            ],
            ContextTarget::TrackLane { .. } => {
                let split_enabled = {
                    let state = &self.timeline.read(cx).state;
                    state.selection.selected_clip_ids.iter().any(|id| {
                        state.find_clip(id).is_some_and(|(_, clip)| {
                            let playhead = state.transport.playhead_beats;
                            matches!(clip.clip_type, ClipType::Audio { .. })
                                && playhead > clip.start_beat + 0.25
                                && playhead < clip.start_beat + clip.duration_beats - 0.25
                        })
                    })
                };
                vec![
                    menu_item_enabled(
                        i18n.tr("context.paste"),
                        "edit:paste",
                        !self.clip_clipboard.is_empty(),
                    ),
                    menu_item_enabled(
                        i18n.tr("context.clip.split-at-playhead"),
                        "clip:split-at-playhead",
                        split_enabled,
                    ),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item(
                        i18n.tr("context.timeline.add-audio"),
                        "track:add-audio",
                    ),
                    ContextMenuEntry::item(i18n.tr("context.timeline.add-midi"), "track:add-midi"),
                    ContextMenuEntry::item(i18n.tr("menu.project-add_bus_track"), "track:add-bus"),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item(i18n.tr("context.zoom-in"), "view:zoom-in"),
                    ContextMenuEntry::item(i18n.tr("context.zoom-out"), "view:zoom-out"),
                ]
            }
            ContextTarget::Clip(clip_id) => {
                let clip_info = self.timeline.read(cx).state.find_clip(clip_id);
                let exists = clip_info.is_some();
                let is_midi =
                    clip_info.is_some_and(|(_, c)| matches!(c.clip_type, ClipType::Midi { .. }));
                let is_audio =
                    clip_info.is_some_and(|(_, c)| matches!(c.clip_type, ClipType::Audio { .. }));
                let reveal_enabled = clip_info.is_some_and(|(_, c)| {
                    matches!(
                        &c.clip_type,
                        ClipType::Audio {
                            source_path: Some(path),
                            ..
                        } if !path.is_empty()
                    )
                });
                let split_enabled = clip_info.is_some_and(|(_, c)| {
                    let state = &self.timeline.read(cx).state;
                    let playhead = state.transport.playhead_beats;
                    is_audio
                        && playhead > c.start_beat + 0.25
                        && playhead < c.start_beat + c.duration_beats - 0.25
                });
                let selected_count = self
                    .timeline
                    .read(cx)
                    .state
                    .selection
                    .selected_clip_ids
                    .len();
                let mut entries = Vec::new();
                if is_midi {
                    entries.push(menu_item_enabled(
                        "Open in Bottom Editor",
                        "editor:open-bottom",
                        exists,
                    ));
                    entries.push(menu_item_enabled(
                        "Open in New MIDI Editor Window",
                        "midi:open-editor",
                        exists,
                    ));
                    entries.push(ContextMenuEntry::Separator);
                }
                entries.push(menu_item_enabled(
                    i18n.tr("context.clip.rename"),
                    "clip:rename",
                    exists,
                ));
                entries.push(
                    menu_item_enabled(
                        i18n.tr("context.clip.duplicate"),
                        "clip:duplicate",
                        exists || selected_count > 0,
                    )
                    .with_shortcut(crate::keymap::accel_display("Ctrl+D")),
                );
                let erase_label = if is_audio {
                    "Erase".to_string()
                } else {
                    i18n.tr("context.clip.delete")
                };
                let erase_command = if is_audio {
                    "clip:erase"
                } else {
                    "clip:delete"
                };
                entries.push(danger_menu_item_enabled(
                    erase_label,
                    erase_command,
                    exists || selected_count > 0,
                ));
                entries.push(ContextMenuEntry::Separator);
                entries.push(menu_item_enabled(
                    i18n.tr("context.clip.split-at-playhead"),
                    "clip:split-at-playhead",
                    split_enabled,
                ));
                if is_audio {
                    entries.push(menu_item_enabled(
                        if exists {
                            i18n.tr("context.clip.reveal-browser")
                        } else {
                            i18n.tr("context.clip.unavailable")
                        },
                        "browser:reveal",
                        reveal_enabled,
                    ));
                    entries.extend(self.ara_clip_menu_entries(clip_id, exists, cx));
                }
                if is_midi {
                    entries.push(menu_item_enabled(
                        i18n.tr("menu.midi-quantize"),
                        "midi:quantize",
                        exists,
                    ));
                    entries.push(menu_item_enabled(
                        "Export MIDI File...",
                        "midi:export-clip",
                        exists,
                    ));
                    // Applies to the notes selected in the MIDI editor.
                    entries.push(ContextMenuEntry::Separator);
                    entries.push(ContextMenuEntry::Header("Articulation".to_string()));
                    for (label, command) in [
                        ("Sustain", "midi:articulation-sustain"),
                        ("Staccato", "midi:articulation-staccato"),
                        ("Staccatissimo", "midi:articulation-staccatissimo"),
                        ("Legato", "midi:articulation-legato"),
                        ("Tenuto", "midi:articulation-tenuto"),
                        ("Accent", "midi:articulation-accent"),
                        ("Marcato", "midi:articulation-marcato"),
                        ("None", "midi:articulation-none"),
                    ] {
                        entries.push(menu_item_enabled(label, command, exists));
                    }
                }
                entries.push(ContextMenuEntry::Separator);
                entries.push(menu_item_enabled(
                    "Clip Properties",
                    "clip:properties",
                    exists,
                ));
                entries
            }
            ContextTarget::Track(track_id) => {
                let track = self.timeline.read(cx).state.find_track(track_id).cloned();
                let exists = track.is_some();
                let mut entries = vec![
                    menu_item_enabled(i18n.tr("context.track.rename"), "track:rename", exists),
                    menu_item_enabled(i18n.tr("context.track.duplicate"), "track:duplicate", false),
                    danger_menu_item_enabled(
                        i18n.tr("context.track.delete"),
                        "track:delete",
                        exists,
                    ),
                    ContextMenuEntry::Separator,
                    menu_item_enabled("Track Color", "track:color", exists),
                    menu_item_enabled("Track Settings", "track:settings", exists),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::Header("Track Height".to_string()),
                    menu_item_enabled("Small", "track:height-small", exists),
                    menu_item_enabled("Normal", "track:height-normal", exists),
                    menu_item_enabled("Large", "track:height-large", exists),
                    menu_item_enabled("Huge", "track:height-huge", exists),
                    menu_item_enabled("Reset Track Height", "track:height-reset", exists),
                    menu_item_enabled("Reset All Track Heights", "track:height-reset-all", exists),
                ];
                // Timebase — what this track's clips hold onto when the tempo
                // moves. Only offered where there are clips to hold: a Bus,
                // Return or Group owns none, so the setting would have nothing
                // to act on and must not pretend otherwise.
                if let Some(track) = track.as_ref().filter(|track| {
                    !track.track_type.is_routing() && track.track_type != TrackType::Master
                }) {
                    let timebase = track.timebase;
                    entries.extend([
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::Header("Timebase".to_string()),
                        ContextMenuEntry::checked_item(
                            "Musical — follows tempo",
                            "track:timebase-musical",
                            timebase == TrackTimebase::Musical,
                        ),
                        ContextMenuEntry::checked_item(
                            "Linear — holds wall-clock time",
                            "track:timebase-linear",
                            timebase == TrackTimebase::Linear,
                        ),
                    ]);
                }
                entries.extend(self.ara_track_menu_entries(track_id, exists, cx));
                entries.extend([
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item(
                        i18n.tr("context.timeline.add-audio"),
                        "track:add-audio",
                    ),
                    ContextMenuEntry::item(i18n.tr("context.timeline.add-midi"), "track:add-midi"),
                    ContextMenuEntry::item(i18n.tr("menu.project-add_bus_track"), "track:add-bus"),
                ]);
                entries
            }
            ContextTarget::TimelineMarker { marker_id, beat } => {
                let state = &self.timeline.read(cx).state;
                let label = state.format_position(*beat as f32);
                let name = state
                    .marker(marker_id)
                    .map(|marker| marker.name.clone())
                    .unwrap_or_else(|| "Marker".to_string());
                vec![
                    ContextMenuEntry::disabled_item(format!("{name} — {label}"), "noop"),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item("Move to Playhead", "marker:move-to-playhead"),
                    ContextMenuEntry::item("Set Playhead to Marker", "marker:goto"),
                    ContextMenuEntry::danger_item("Delete Marker", "marker:delete"),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item(i18n.tr("menu.project-add_marker"), "ruler:add-marker"),
                    ContextMenuEntry::item("Add Tempo Marker", "ruler:add-tempo-marker"),
                    ContextMenuEntry::item("Add Time Signature Marker", "ruler:add-ts-marker"),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item(i18n.tr("context.zoom-in"), "view:zoom-in"),
                    ContextMenuEntry::item(i18n.tr("context.zoom-out"), "view:zoom-out"),
                ]
            }
            ContextTarget::SongTextMarker { event_id, beat } => {
                let state = &self.timeline.read(cx).state;
                let position = state.format_position_at(*beat);
                let event_label = state
                    .song_text_event(event_id)
                    .map(|event| format!("{}: {}", event.event_type().label(), event.text()))
                    .unwrap_or_else(|| "Song Text event".to_string());
                vec![
                    ContextMenuEntry::disabled_item(format!("{event_label} at {position}"), "noop"),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item("Edit", "panel:show-lyric-editor"),
                    ContextMenuEntry::item("Move to Playhead", "song_text.move_to_playhead"),
                    danger_menu_item_enabled(
                        i18n.tr("context.clip.delete"),
                        "song_text.delete_selected",
                        state.song_text_event(event_id).is_some(),
                    ),
                ]
            }
            ContextTarget::AutomationLane { .. } => vec![
                ContextMenuEntry::item("Cycle Automation Target", "automation:cycle-target"),
                ContextMenuEntry::item("Select All Points", "automation:select-all-points"),
                ContextMenuEntry::item("Clear Selection", "automation:clear-selection"),
                ContextMenuEntry::Separator,
                ContextMenuEntry::item(i18n.tr("context.zoom-in"), "view:zoom-in"),
                ContextMenuEntry::item(i18n.tr("context.zoom-out"), "view:zoom-out"),
            ],
            ContextTarget::Browser(path_opt) => {
                let mut entries = Vec::new();
                if let Some(path) = path_opt {
                    // Directory-ness comes from the browser's own cached node.
                    // `path.is_dir()` here was a blocking `stat` run while
                    // *building the menu*, i.e. on every right-click, which
                    // stalls on a network share or a spun-down disk.
                    let is_directory = self.file_browser.known_is_directory(path).unwrap_or(false);
                    if is_directory {
                        let is_drive = path.parent().is_none();
                        if is_drive {
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.open-folder"),
                                "browser:reveal",
                            ));
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.refresh"),
                                "browser:refresh",
                            ));
                        } else {
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.open"),
                                "browser:open",
                            ));
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.reveal"),
                                "browser:reveal",
                            ));
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.refresh"),
                                "browser:refresh",
                            ));
                            // "New Folder" and "Rename" used to sit here as
                            // permanently-disabled items backed by `eprintln!`
                            // stubs. A menu entry that can never do anything is
                            // not an affordance, so they are gone.
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.copy-path"),
                                "browser:copy-path",
                            ));
                        }
                    } else {
                        let ext = path
                            .extension()
                            .and_then(|s| s.to_str())
                            .map(|s| s.to_ascii_lowercase())
                            .unwrap_or_default();

                        if is_supported_audio_ext(&ext) {
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.import"),
                                "browser:import",
                            ));
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.reveal"),
                                "browser:reveal",
                            ));
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.copy-path"),
                                "browser:copy-path",
                            ));
                        } else if ext == "fbproj" {
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.project.open"),
                                "project:open",
                            ));
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.reveal"),
                                "browser:reveal",
                            ));
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.copy-path"),
                                "browser:copy-path",
                            ));
                        } else {
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.reveal"),
                                "browser:reveal",
                            ));
                            entries.push(ContextMenuEntry::item(
                                i18n.tr("context.browser.copy-path"),
                                "browser:copy-path",
                            ));
                        }
                    }
                } else {
                    entries.push(ContextMenuEntry::disabled_item(
                        i18n.tr("context.browser.no-selection"),
                        "noop",
                    ));
                }
                entries
            }
            ContextTarget::Mixer(_) => vec![
                ContextMenuEntry::item("Add Bus", "mixer:create-bus"),
                ContextMenuEntry::Separator,
                ContextMenuEntry::item(i18n.tr("context.mixer.reset-volume"), "mixer:reset-volume"),
                ContextMenuEntry::item(i18n.tr("context.mixer.reset-pan"), "mixer:reset-pan"),
                ContextMenuEntry::Separator,
                ContextMenuEntry::item(i18n.tr("context.track.mute"), "track:mute"),
                ContextMenuEntry::item(i18n.tr("context.track.solo"), "track:solo"),
                ContextMenuEntry::Separator,
                ContextMenuEntry::item("Track Color", "track:color"),
                ContextMenuEntry::danger_item(i18n.tr("context.track.delete"), "track:delete"),
            ],
            ContextTarget::SendPicker { track_id } => {
                let state = &self.timeline.read(cx).state;
                let Some(source) = state.find_track(track_id) else {
                    return vec![ContextMenuEntry::disabled_item("Track unavailable", "noop")];
                };
                let mut entries = vec![ContextMenuEntry::Header("Send To".to_string())];
                let existing: std::collections::HashSet<&str> = source
                    .sends
                    .iter()
                    .map(|send| send.target_track_id.as_str())
                    .collect();
                let mut available = 0usize;
                for target in state
                    .tracks
                    .iter()
                    .filter(|target| target.id != *track_id && is_project_routing_track(target))
                {
                    let type_label = match target.track_type {
                        TrackType::Bus => "Bus",
                        TrackType::Return => "Return",
                        _ => "",
                    };
                    if existing.contains(target.id.as_str()) {
                        entries.push(ContextMenuEntry::disabled_item(
                            format!("{} ({type_label}) - already sending", target.name),
                            "noop",
                        ));
                    } else {
                        available += 1;
                        entries.push(ContextMenuEntry::item(
                            format!("{} ({type_label})", target.name),
                            format!("mixer:add-send-to:{}:{}", track_id, target.id),
                        ));
                    }
                }
                if available == 0 {
                    entries.push(ContextMenuEntry::disabled_item(
                        "No available bus/return",
                        "noop",
                    ));
                }
                entries.push(ContextMenuEntry::Separator);
                entries.push(ContextMenuEntry::item(
                    "Create Return and Send",
                    format!("mixer:create-return-send:{track_id}"),
                ));
                entries
            }
            ContextTarget::OutputPicker { track_id } => {
                use crate::components::timeline::timeline_state::TrackOutputRouting;
                let state = &self.timeline.read(cx).state;
                let Some(source) = state.find_track(track_id) else {
                    return vec![ContextMenuEntry::disabled_item("Track unavailable", "noop")];
                };
                let current = source.routing.output.clone();
                let mut entries = vec![ContextMenuEntry::Header("Output".to_string())];
                entries.push(ContextMenuEntry::checked_item(
                    "Main".to_string(),
                    format!("mixer:set-output:{track_id}:main"),
                    matches!(current, TrackOutputRouting::Main),
                ));
                let mut bus_count = 0usize;
                for target in state
                    .tracks
                    .iter()
                    .filter(|target| target.id != *track_id && is_project_routing_track(target))
                {
                    let type_label = match target.track_type {
                        TrackType::Bus => "Bus",
                        TrackType::Return => "Return",
                        TrackType::Group => "Group",
                        _ => "",
                    };
                    bus_count += 1;
                    let checked = matches!(
                        &current,
                        TrackOutputRouting::Bus { bus_id } if bus_id == &target.id
                    );
                    entries.push(ContextMenuEntry::checked_item(
                        if type_label.is_empty() {
                            target.name.clone()
                        } else {
                            format!("{} ({type_label})", target.name)
                        },
                        format!("mixer:set-output:{track_id}:bus:{}", target.id),
                        checked,
                    ));
                }
                if bus_count == 0 {
                    entries.push(ContextMenuEntry::disabled_item(
                        "No bus/return available",
                        "noop",
                    ));
                }
                entries
            }
            // Master / Monitor output pickers. Both list Output Audio
            // Connections only — no input bus, no hardware port — and both end
            // with the escape hatch into Audio Connections.
            ContextTarget::MasterOutputPicker | ContextTarget::MonitorOutputPicker => {
                use crate::output_routing::{
                    is_current_choice, master_output_options, monitor_output_options,
                    output_command_id, OutputChoice, MASTER_OUTPUT_COMMAND_PREFIX,
                    MONITOR_OUTPUT_COMMAND_PREFIX,
                };
                let is_master = matches!(target, ContextTarget::MasterOutputPicker);
                let state = &self.timeline.read(cx).state;
                let current = if is_master {
                    state.master_output_connection_id.as_ref()
                } else {
                    state.monitor_output_connection_id.as_ref()
                };
                let (prefix, options, header) = if is_master {
                    (
                        MASTER_OUTPUT_COMMAND_PREFIX,
                        master_output_options(&state.audio_connections, current),
                        "Master Output",
                    )
                } else {
                    (
                        MONITOR_OUTPUT_COMMAND_PREFIX,
                        monitor_output_options(&state.audio_connections, current),
                        "Monitor Output",
                    )
                };

                let mut entries = vec![ContextMenuEntry::Header(header.to_string())];
                for choice in &options {
                    if matches!(choice, OutputChoice::OpenAudioConnections) {
                        entries.push(ContextMenuEntry::Separator);
                    }
                    entries.push(ContextMenuEntry::checked_item(
                        choice.label(),
                        output_command_id(prefix, choice),
                        is_current_choice(choice, current),
                    ));
                }
                entries
            }
            ContextTarget::AutomationTargetPicker { track_id } => {
                use crate::components::timeline::timeline_state::{
                    automation_target_menu_command, AutomationTarget,
                };
                let state = &self.timeline.read(cx).state;
                let Some(track) = state.find_track(track_id) else {
                    return vec![ContextMenuEntry::disabled_item("Track unavailable", "noop")];
                };
                let existing: std::collections::HashSet<_> = track
                    .automation_lanes
                    .iter()
                    .map(|lane| lane.target.clone())
                    .collect();
                let mut entries = vec![ContextMenuEntry::Header("Add Automation".to_string())];

                let push_group = |entries: &mut Vec<ContextMenuEntry>,
                                  header: &str,
                                  targets: &[AutomationTarget]| {
                    if targets.is_empty() {
                        return;
                    }
                    entries.push(ContextMenuEntry::Header(header.to_string()));
                    for target in targets {
                        let label = target.display_name();
                        if existing.contains(target) {
                            entries.push(ContextMenuEntry::disabled_item(
                                format!("{label} (already added)"),
                                "noop",
                            ));
                        } else {
                            entries.push(ContextMenuEntry::item(
                                label,
                                automation_target_menu_command(track_id, target),
                            ));
                        }
                    }
                };

                push_group(
                    &mut entries,
                    "Track",
                    &[
                        AutomationTarget::TrackVolume,
                        AutomationTarget::TrackPan,
                        AutomationTarget::TrackMute,
                    ],
                );

                let plugin_targets: Vec<AutomationTarget> = state
                    .available_automation_targets(track_id)
                    .into_iter()
                    .filter(|t| matches!(t, AutomationTarget::PluginParameter { .. }))
                    .collect();
                push_group(&mut entries, "Plugins", &plugin_targets);

                let send_targets: Vec<AutomationTarget> = state
                    .available_automation_targets(track_id)
                    .into_iter()
                    .filter(|t| matches!(t, AutomationTarget::SendLevel { .. }))
                    .collect();
                push_group(&mut entries, "Sends", &send_targets);

                if entries.len() == 1 {
                    entries.push(ContextMenuEntry::disabled_item(
                        "No automation targets available",
                        "noop",
                    ));
                }
                entries
            }
            ContextTarget::TimeSignature => {
                let state = &self.timeline.read(cx).state;
                let pt = state.time_signature_at_playhead();
                let label = pt.label();
                let has_markers = state.time_signature_has_markers();
                let mut entries = vec![
                    ContextMenuEntry::disabled_item(format!("Time Signature: {label}"), "noop"),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item(
                        "Add Time Signature Marker at Playhead",
                        "ts:add-marker",
                    ),
                    ContextMenuEntry::item("Edit Current Time Signature…", "ts:edit"),
                    ContextMenuEntry::Separator,
                ];
                if has_markers {
                    entries.push(ContextMenuEntry::danger_item(
                        "Clear Time Signature Markers",
                        "ts:clear",
                    ));
                    entries.push(ContextMenuEntry::Separator);
                }
                if state.show_time_signature_track {
                    entries.push(ContextMenuEntry::item(
                        "Hide Time Signature Track",
                        "ts:hide-track",
                    ));
                } else {
                    entries.push(ContextMenuEntry::item(
                        "Show Time Signature Track",
                        "ts:open-track",
                    ));
                }
                entries
            }
            ContextTarget::TimeSignaturePoint { point_id, beat } => {
                let state = &self.timeline.read(cx).state;
                let label = state.format_position_at(*beat);
                let sig = state
                    .time_signature_map
                    .points
                    .iter()
                    .find(|p| p.id == *point_id)
                    .map(|p| p.label())
                    .unwrap_or_else(|| "4/4".to_string());
                vec![
                    ContextMenuEntry::disabled_item(
                        format!("Time signature: {sig} at {label}"),
                        "noop",
                    ),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item("Edit Time Signature…", "ts:edit"),
                    ContextMenuEntry::item("Delete Time Signature Marker", "ts:delete-point"),
                    ContextMenuEntry::item("Move to Playhead", "ts:move-to-playhead"),
                ]
            }
            ContextTarget::TimeSignatureTrack { beat, point_id } => {
                let state = &self.timeline.read(cx).state;
                let label = state.format_position_at(*beat);
                if point_id.is_some() {
                    let sig = point_id
                        .as_ref()
                        .and_then(|id| {
                            state
                                .time_signature_map
                                .points
                                .iter()
                                .find(|p| p.id == *id)
                                .map(|p| p.label())
                        })
                        .unwrap_or_else(|| "4/4".to_string());
                    vec![
                        ContextMenuEntry::disabled_item(
                            format!("Time signature: {sig} at {label}"),
                            "noop",
                        ),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::item("Edit Time Signature…", "ts:edit"),
                        ContextMenuEntry::item("Delete Time Signature Marker", "ts:delete-point"),
                        ContextMenuEntry::item("Move to Playhead", "ts:move-to-playhead"),
                    ]
                } else {
                    let sig = state
                        .time_signature_map
                        .time_signature_at_beat(*beat)
                        .label();
                    vec![
                        ContextMenuEntry::disabled_item(
                            format!("Time signature at {label}: {sig}"),
                            "noop",
                        ),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::item(
                            "Add Time Signature Marker Here",
                            "ts:add-point-here",
                        ),
                        ContextMenuEntry::item("Edit Time Signature…", "ts:edit"),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::item("Show Time Signature Track", "ts:open-track"),
                        ContextMenuEntry::item("Hide Time Signature Track", "ts:hide-track"),
                    ]
                }
            }
            ContextTarget::Tempo => {
                let state = &self.timeline.read(cx).state;
                let bpm = state.effective_bpm_at_playhead();
                let has_automation = state.tempo_has_automation();
                let has_tap_session = self.tap_tempo.tap_count() > 0;
                let mut entries = vec![
                    ContextMenuEntry::disabled_item(format!("Tempo: {bpm:.1} BPM"), "noop"),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item("Tap Tempo", "tempo:tap"),
                    ContextMenuEntry::item("Add Current Tempo at Playhead", "tempo:add-tap-marker"),
                    menu_item_enabled("Reset Tap Session", "tempo:reset-tap", has_tap_session),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item("Add Tempo Point at Playhead", "tempo:add-marker"),
                    ContextMenuEntry::item("Edit Current BPM…", "tempo:edit-bpm"),
                ];
                if has_automation {
                    entries.push(ContextMenuEntry::item("Fit Tempo Range", "tempo:fit-range"));
                    entries.push(ContextMenuEntry::danger_item(
                        "Clear Tempo Automation",
                        "tempo:clear",
                    ));
                } else {
                    entries.push(ContextMenuEntry::item(
                        "Create Tempo Automation",
                        "tempo:create",
                    ));
                }
                entries.push(ContextMenuEntry::Separator);
                if state.show_tempo_track {
                    entries.push(ContextMenuEntry::item(
                        "Hide Tempo Track",
                        "tempo:hide-track",
                    ));
                } else {
                    entries.push(ContextMenuEntry::item(
                        "Show Tempo Track",
                        "tempo:open-track",
                    ));
                }
                entries
            }
            ContextTarget::TapTempo => {
                let has_tap_session = self.tap_tempo.tap_count() > 0;
                let mut entries = vec![ContextMenuEntry::item("Tap", "tempo:tap")];
                entries.push(ContextMenuEntry::item(
                    "Add Current Tempo at Playhead",
                    "tempo:add-tap-marker",
                ));
                entries.push(menu_item_enabled(
                    "Reset Tap Session",
                    "tempo:reset-tap",
                    has_tap_session,
                ));
                entries
            }
            ContextTarget::CountIn => {
                let bars = self
                    .settings
                    .read(cx)
                    .current
                    .recording
                    .metronome
                    .count_in_bars;
                let mut entries = vec![ContextMenuEntry::Header("Record Count-in".to_string())];
                for value in 1..=4 {
                    entries.push(ContextMenuEntry::checked_item(
                        format!("{value} bar{}", if value == 1 { "" } else { "s" }),
                        format!("recording:set-count-in-bars:{value}"),
                        bars == value,
                    ));
                }
                entries
            }
            ContextTarget::TempoTrack {
                beat,
                bpm,
                point_id,
            } => {
                let state = &self.timeline.read(cx).state;
                let label = state.format_position(*beat as f32);
                if let Some(id) = point_id.as_deref() {
                    let bpm_label = if bpm.fract().abs() < 0.05 {
                        format!("{bpm:.0}")
                    } else {
                        format!("{bpm:.1}")
                    };
                    // Which shape this marker is already on. Three plain items
                    // gave no way to read the current one, which is how a lane
                    // with working curves still feels like it does nothing.
                    let curve = state
                        .tempo_map
                        .points
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.curve)
                        .unwrap_or_default();
                    vec![
                        ContextMenuEntry::disabled_item(
                            format!("Tempo point: {bpm_label} BPM at {label}"),
                            "noop",
                        ),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::item("Edit BPM…", "tempo:edit-bpm"),
                        ContextMenuEntry::item("Delete Tempo Point", "tempo:delete-point"),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::Header("Curve to next marker".to_string()),
                        ContextMenuEntry::checked_item(
                            "Hold",
                            "tempo:curve-hold",
                            curve == TempoCurve::Hold,
                        ),
                        ContextMenuEntry::checked_item(
                            "Linear",
                            "tempo:curve-linear",
                            curve == TempoCurve::Linear,
                        ),
                        ContextMenuEntry::checked_item(
                            "Smooth",
                            "tempo:curve-smooth",
                            curve == TempoCurve::Smooth,
                        ),
                    ]
                } else {
                    let bpm_label = if bpm.fract().abs() < 0.05 {
                        format!("{bpm:.0}")
                    } else {
                        format!("{bpm:.1}")
                    };
                    vec![
                        ContextMenuEntry::disabled_item(
                            format!("Tempo at {label}: {bpm_label} BPM"),
                            "noop",
                        ),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::item("Add Tempo Point Here", "tempo:add-point-here"),
                        ContextMenuEntry::item("Set Fixed Tempo From Here", "tempo:set-fixed-here"),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::item("Hide Tempo Track", "tempo:hide-track"),
                    ]
                }
            }
            ContextTarget::MarkerTrack { beat, marker_id } => {
                let state = &self.timeline.read(cx).state;
                let label = state.format_position(*beat as f32);
                match marker_id.as_deref().and_then(|id| state.marker(id)) {
                    Some(marker) => {
                        let at = state.format_position(marker.beat as f32);
                        vec![
                            ContextMenuEntry::disabled_item(
                                format!("{} — {at}", marker.name),
                                "noop",
                            ),
                            ContextMenuEntry::Separator,
                            ContextMenuEntry::item("Move to Playhead", "marker:move-to-playhead"),
                            ContextMenuEntry::item("Set Playhead to Marker", "marker:goto"),
                            ContextMenuEntry::Separator,
                            ContextMenuEntry::danger_item("Delete Marker", "marker:delete"),
                        ]
                    }
                    None => vec![
                        ContextMenuEntry::disabled_item(format!("Marker lane at {label}"), "noop"),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::item("Add Marker Here", "marker:add-here"),
                        ContextMenuEntry::item("Add Marker at Playhead", "marker:add-at-playhead"),
                        ContextMenuEntry::Separator,
                        danger_menu_item_enabled(
                            "Delete All Markers",
                            "marker:clear-all",
                            !state.markers.is_empty(),
                        ),
                        ContextMenuEntry::item("Hide Marker Track", "marker:hide-track"),
                    ],
                }
            }
            ContextTarget::MarkerLane => {
                let state = &self.timeline.read(cx).state;
                vec![
                    ContextMenuEntry::Header("Marker Track".to_string()),
                    ContextMenuEntry::item("Add Marker at Playhead", "marker:add-at-playhead"),
                    ContextMenuEntry::Separator,
                    danger_menu_item_enabled(
                        "Delete All Markers",
                        "marker:clear-all",
                        !state.markers.is_empty(),
                    ),
                    ContextMenuEntry::item("Hide Marker Track", "marker:hide-track"),
                ]
            }
            ContextTarget::RegionTrack { beat, region_id } => {
                let state = &self.timeline.read(cx).state;
                let label = state.format_position(*beat as f32);
                match region_id.as_deref().and_then(|id| state.region(id)) {
                    Some(region) => {
                        let (start, end) = region.normalized_range();
                        let span = format!(
                            "{} – {}",
                            state.format_position(start as f32),
                            state.format_position(end as f32)
                        );
                        vec![
                            ContextMenuEntry::disabled_item(
                                format!("{} — {span}", region.name),
                                "noop",
                            ),
                            ContextMenuEntry::Separator,
                            ContextMenuEntry::item("Set Loop to Region", "region:set-loop"),
                            ContextMenuEntry::item("Move to Playhead", "region:move-to-playhead"),
                            ContextMenuEntry::Separator,
                            ContextMenuEntry::danger_item("Delete Region", "region:delete"),
                        ]
                    }
                    None => vec![
                        ContextMenuEntry::disabled_item(format!("Region lane at {label}"), "noop"),
                        ContextMenuEntry::Separator,
                        ContextMenuEntry::item("Create Region Here", "region:add-here"),
                        ContextMenuEntry::item(
                            "Create Region at Playhead",
                            "region:add-at-playhead",
                        ),
                        ContextMenuEntry::Separator,
                        danger_menu_item_enabled(
                            "Delete All Regions",
                            "region:clear-all",
                            !state.regions.is_empty(),
                        ),
                        ContextMenuEntry::item("Hide Region Track", "region:hide-track"),
                    ],
                }
            }
            ContextTarget::RegionLane => {
                let state = &self.timeline.read(cx).state;
                vec![
                    ContextMenuEntry::Header("Region Track".to_string()),
                    ContextMenuEntry::item("Create Region at Playhead", "region:add-at-playhead"),
                    ContextMenuEntry::Separator,
                    danger_menu_item_enabled(
                        "Delete All Regions",
                        "region:clear-all",
                        !state.regions.is_empty(),
                    ),
                    ContextMenuEntry::item("Hide Region Track", "region:hide-track"),
                ]
            }
            ContextTarget::TimelineRuler { beat } => {
                let label = self.timeline.read(cx).state.format_position(*beat as f32);
                let has_automation = self.timeline.read(cx).state.tempo_has_automation();
                let mut entries = vec![
                    ContextMenuEntry::disabled_item(format!("Tempo at {label}"), "noop"),
                    ContextMenuEntry::Separator,
                    ContextMenuEntry::item("Add Marker Here", "ruler:add-marker"),
                    ContextMenuEntry::item("Create Region Here", "ruler:add-region"),
                    ContextMenuEntry::item("Set Loop to Selection", "ruler:set-loop-selection"),
                    ContextMenuEntry::Separator,
                ];
                if !has_automation {
                    entries.push(ContextMenuEntry::item(
                        "Create Tempo Automation Here",
                        "ruler:create-tempo-here",
                    ));
                }
                entries.push(ContextMenuEntry::item(
                    "Add Tempo Marker Here",
                    "ruler:add-tempo-marker",
                ));
                entries.push(ContextMenuEntry::Separator);
                entries.push(ContextMenuEntry::item(
                    "Add Time Signature Marker Here",
                    "ruler:add-ts-marker",
                ));
                entries.push(ContextMenuEntry::item("Edit Time Signature…", "ts:edit"));
                entries.push(ContextMenuEntry::Separator);
                // The conductor lanes are on by default now, so these have to
                // reflect what is actually on screen — a permanent "Show …"
                // entry on a lane that is already visible does nothing when
                // picked, which is exactly the kind of dead control the design
                // contract forbids.
                let st = self.timeline.read(cx);
                entries.push(if st.state.show_tempo_track {
                    ContextMenuEntry::item("Hide Tempo Track", "tempo:hide-track")
                } else {
                    ContextMenuEntry::item("Show Tempo Track", "tempo:open-track")
                });
                entries.push(if st.state.show_time_signature_track {
                    ContextMenuEntry::item("Hide Time Signature Track", "ts:hide-track")
                } else {
                    ContextMenuEntry::item("Show Time Signature Track", "ts:open-track")
                });
                entries.push(if st.state.show_marker_track {
                    ContextMenuEntry::item("Hide Marker Track", "marker:hide-track")
                } else {
                    ContextMenuEntry::item("Show Marker Track", "marker:open-track")
                });
                entries.push(if st.state.show_region_track {
                    ContextMenuEntry::item("Hide Region Track", "region:hide-track")
                } else {
                    ContextMenuEntry::item("Show Region Track", "region:open-track")
                });
                entries.push(if st.state.show_song_text_track {
                    ContextMenuEntry::item("Hide Song Text Track", "songtext:hide-track")
                } else {
                    ContextMenuEntry::item("Show Song Text Track", "songtext:open-track")
                });
                entries
            }
        }
    }

    /// Beat under the cursor for the active Marker lane context menu, if any.
    pub(super) fn marker_context_beat(&self) -> Option<f64> {
        match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(ContextTarget::MarkerTrack { beat, .. }) => Some(*beat),
                _ => None,
            },
            _ => None,
        }
    }

    /// Marker the active context menu is acting on: the one under the cursor in
    /// the lane, the one right-clicked in the ruler, else the lane selection.
    pub(super) fn marker_context_id(&self, cx: &App) -> Option<String> {
        let from_menu = match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(ContextTarget::MarkerTrack { marker_id, .. }) => {
                    marker_id.clone()
                }
                ContextMenuTarget::Extended(ContextTarget::TimelineMarker {
                    marker_id, ..
                }) => Some(marker_id.clone()),
                _ => None,
            },
            _ => None,
        };
        from_menu.or_else(|| self.timeline.read(cx).state.selected_marker_id.clone())
    }

    /// Beat under the cursor for the active Region lane context menu, if any.
    pub(super) fn region_context_beat(&self) -> Option<f64> {
        match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(ContextTarget::RegionTrack { beat, .. }) => Some(*beat),
                _ => None,
            },
            _ => None,
        }
    }

    /// Region the active context menu is acting on, else the lane selection.
    pub(super) fn region_context_id(&self, cx: &App) -> Option<String> {
        let from_menu = match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(ContextTarget::RegionTrack { region_id, .. }) => {
                    region_id.clone()
                }
                _ => None,
            },
            _ => None,
        };
        from_menu.or_else(|| self.timeline.read(cx).state.selected_region_id.clone())
    }

    /// Beat under the cursor for the active timeline-ruler context menu, if any.
    pub(super) fn ruler_context_beat(&self) -> Option<f64> {
        match &self.overlay.open_popover {
            Some(OpenPopover::Context { request }) => match &request.target {
                ContextMenuTarget::Extended(ContextTarget::TimelineRuler { beat }) => Some(*beat),
                _ => None,
            },
            _ => None,
        }
    }

    /// The main-window text field that currently owns the keyboard, if any.
    ///
    /// OS IME composition is routed to exactly this field via the root-level
    /// [`crate::components::text_input::ime_input_bridge`]. The numeric tempo /
    /// time-signature inline editors are intentionally excluded — they accept
    /// ASCII digits only and run through their own key handlers.
    pub(super) fn focused_text_target(&self, window: &Window) -> Option<TextMenuTarget> {
        // PluginPickerSearch is intentionally absent: that field is also hosted by
        // the separate InsertPickerWindow, which can't share this bridge, so it
        // stays on the key_char insertion path in both hosts (see
        // `handle_plugin_picker_text_input`).
        const TARGETS: [TextMenuTarget; 7] = [
            TextMenuTarget::CommandPalette,
            TextMenuTarget::ProjectSwitcherSearch,
            TextMenuTarget::BrowserSearch,
            TextMenuTarget::AutomationPickerSearch,
            TextMenuTarget::InspectorName,
            TextMenuTarget::InspectorClipName,
            TextMenuTarget::InspectorColorHex,
        ];
        TARGETS
            .into_iter()
            .find(|&target| self.text_input(target).is_focused(window))
    }

    /// Focus handle of the currently-focused main-window text field, so the
    /// render pass can mount the IME bridge against it.
    pub(super) fn focused_text_input_handle(&self, window: &Window) -> Option<FocusHandle> {
        self.focused_text_target(window)
            .map(|target| self.text_input(target).focus_handle.clone())
    }

    /// After an IME edit mutates a field, propagate the new value exactly like the
    /// raw key path does: search fields refresh their filter/query; inspector name
    /// fields live-commit to the bound track/clip.
    fn after_ime_edit(&mut self, target: TextMenuTarget, cx: &mut Context<Self>) {
        match target {
            TextMenuTarget::InspectorName => self.commit_inspector_name(cx),
            TextMenuTarget::InspectorClipName => self.commit_inspector_clip_name(cx),
            TextMenuTarget::InspectorColorHex => {}
            other => self.sync_text_input_target(other),
        }
    }
}

/// Systemwide IME bridge for the main window. Every platform IME call is routed
/// to whichever main-window text field currently holds focus, so CJK/Thai
/// composition, dead keys, and pasted Unicode reach browser search, the command
/// palette, the project switcher, plugin search, and inspector name fields — not
/// just the bridged external dialogs. Mirrors `impl_single_input_window_ime!`,
/// but resolves the target field by focus instead of naming one.
impl EntityInputHandler for StudioLayout {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let target = self.focused_text_target(window)?;
        self.text_input_mut(target)
            .text_for_utf16_range(range, actual_range)
    }

    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let target = self.focused_text_target(window)?;
        self.text_input_mut(target)
            .selected_text_range_utf16(ignore_disabled_input)
    }

    fn marked_text_range(
        &self,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let target = self.focused_text_target(window)?;
        self.text_input(target).marked_text_range_utf16()
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(target) = self.focused_text_target(window) {
            self.text_input_mut(target).unmark_text();
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.focused_text_target(window) {
            self.text_input_mut(target)
                .replace_text_in_utf16_range(range, text);
            self.after_ime_edit(target, cx);
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.focused_text_target(window) {
            self.text_input_mut(target)
                .replace_and_mark_text_in_utf16_range(range, new_text, new_selected_range);
            self.after_ime_edit(target, cx);
            cx.notify();
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // NOTE: `bounds` is the full-window IME layer rect, so the candidate
        // window anchors approximately (the field's real rect lives behind a
        // panel function). Composition/commit are unaffected; only the candidate
        // popup placement is coarse for main-window fields.
        let target = self.focused_text_target(window)?;
        self.text_input_mut(target)
            .bounds_for_utf16_range(range_utf16, bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

//! The project key: root and scale, set from the transport readout or
//! Project Settings, saved with the project and undoable.

use gpui::{Context, Window};

use crate::components::context_menu::ContextMenuEntry;
use crate::components::edit::EditCommand;
use crate::components::timeline::timeline_state::{MidiScale, ScaleKind, ScaleRoot};

use super::{ContextMenuRequest, ContextMenuTarget, ContextTarget, StudioLayout};

/// Scale a key gets when only its root is picked.
const DEFAULT_KEY_KIND: ScaleKind = ScaleKind::Major;
/// Root a key gets when only its scale is picked.
const DEFAULT_KEY_ROOT: ScaleRoot = ScaleRoot::C;

/// What a `key:*` command does to the current key.
fn apply_key_command(current: Option<MidiScale>, command_id: &str) -> Option<Option<MidiScale>> {
    if command_id == "key:clear" {
        return Some(None);
    }
    if let Some(value) = command_id.strip_prefix("key:root:") {
        let root = ScaleRoot::from_pitch_class(value.parse::<u8>().ok()?);
        let kind = current.map(|key| key.kind).unwrap_or(DEFAULT_KEY_KIND);
        return Some(Some(MidiScale::new(root, kind)));
    }
    if let Some(value) = command_id.strip_prefix("key:scale:") {
        let kind = ScaleKind::from_tag(value.parse::<u8>().ok()?)?;
        if kind == ScaleKind::Chromatic {
            return Some(None);
        }
        let root = current.map(|key| key.root).unwrap_or(DEFAULT_KEY_ROOT);
        return Some(Some(MidiScale::new(root, kind)));
    }
    None
}

impl StudioLayout {
    /// Set (or clear) the project key as one undo step.
    pub(crate) fn set_project_key(&mut self, key: Option<MidiScale>, cx: &mut Context<Self>) {
        let key = key.filter(|key| key.kind != ScaleKind::Chromatic);
        let changed = self.timeline.update(cx, |timeline, cx| {
            let prev = timeline.state.project_key;
            if prev == key {
                return false;
            }
            timeline.state.project_key = key;
            timeline.record_executed_command(EditCommand::SetProjectKey { prev, next: key }, cx);
            true
        });
        if changed {
            self.mark_dirty();
            self.push_project_settings_snapshot_to_window(cx);
            cx.notify();
        }
    }

    /// Run a `key:root:<pitch class>`, `key:scale:<tag>` or `key:clear`
    /// command. Returns whether `command_id` was one.
    pub(super) fn handle_project_key_command(
        &mut self,
        command_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !command_id.starts_with("key:") {
            return false;
        }
        let current = self.timeline.read(cx).state.project_key;
        if let Some(next) = apply_key_command(current, command_id) {
            self.set_project_key(next, cx);
        }
        true
    }

    /// Open the root (`root == true`) or scale picker under the transport's
    /// key readout.
    pub(super) fn open_project_key_menu(
        &mut self,
        window: &Window,
        x: f32,
        y: f32,
        root: bool,
        cx: &mut Context<Self>,
    ) {
        let target = if root {
            ContextTarget::ProjectKeyRoot
        } else {
            ContextTarget::ProjectKeyScale
        };
        self.try_open_context_menu(
            ContextMenuRequest::from_window(window, x, y, ContextMenuTarget::Extended(target)),
            cx,
        );
    }

    pub(super) fn project_key_menu_entries(
        &self,
        root: bool,
        cx: &gpui::App,
    ) -> Vec<ContextMenuEntry> {
        let current = self.timeline.read(cx).state.project_key;
        let mut entries = Vec::new();
        if root {
            entries.push(ContextMenuEntry::Header("Key Root".to_string()));
            entries.extend(ScaleRoot::ALL.iter().map(|root| {
                ContextMenuEntry::checked_item(
                    root.label(),
                    format!("key:root:{}", root.pitch_class()),
                    current.is_some_and(|key| key.root == *root),
                )
            }));
        } else {
            entries.push(ContextMenuEntry::Header("Scale".to_string()));
            entries.extend(MidiScale::KEY_KINDS.iter().map(|kind| {
                ContextMenuEntry::checked_item(
                    kind.label(),
                    format!("key:scale:{}", kind.to_tag()),
                    current.is_some_and(|key| key.kind == *kind),
                )
            }));
        }
        entries.push(ContextMenuEntry::Separator);
        entries.push(ContextMenuEntry::checked_item(
            "No Key",
            "key:clear",
            current.is_none(),
        ));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picking_a_root_keeps_the_scale_and_starts_major() {
        assert_eq!(
            apply_key_command(None, "key:root:9"),
            Some(Some(MidiScale::new(ScaleRoot::A, ScaleKind::Major)))
        );
        let dorian = Some(MidiScale::new(ScaleRoot::D, ScaleKind::Dorian));
        assert_eq!(
            apply_key_command(dorian, "key:root:7"),
            Some(Some(MidiScale::new(ScaleRoot::G, ScaleKind::Dorian)))
        );
    }

    #[test]
    fn picking_a_scale_keeps_the_root_and_starts_on_c() {
        let minor = ScaleKind::NaturalMinor.to_tag();
        assert_eq!(
            apply_key_command(None, &format!("key:scale:{minor}")),
            Some(Some(MidiScale::new(ScaleRoot::C, ScaleKind::NaturalMinor)))
        );
        let e_major = Some(MidiScale::new(ScaleRoot::E, ScaleKind::Major));
        assert_eq!(
            apply_key_command(e_major, &format!("key:scale:{minor}")),
            Some(Some(MidiScale::new(ScaleRoot::E, ScaleKind::NaturalMinor)))
        );
    }

    #[test]
    fn a_key_edit_undoes_and_redoes() {
        use crate::components::timeline::timeline_state::TimelineState;

        let mut state = TimelineState::default();
        let a_minor = Some(MidiScale::new(ScaleRoot::A, ScaleKind::NaturalMinor));
        let command = EditCommand::SetProjectKey {
            prev: None,
            next: a_minor,
        };
        command.execute(&mut state);
        assert_eq!(state.project_key, a_minor);
        command.undo(&mut state);
        assert_eq!(state.project_key, None);
        command.execute(&mut state);
        assert_eq!(state.project_key, a_minor);
    }

    #[test]
    fn clearing_and_malformed_commands() {
        let key = Some(MidiScale::new(ScaleRoot::E, ScaleKind::Major));
        assert_eq!(apply_key_command(key, "key:clear"), Some(None));
        assert_eq!(apply_key_command(key, "key:scale:0"), Some(None));
        assert_eq!(apply_key_command(key, "key:root:x"), None);
        assert_eq!(apply_key_command(key, "key:scale:250"), None);
        assert_eq!(apply_key_command(key, "ts:edit"), None);
    }
}

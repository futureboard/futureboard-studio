//! The piano roll's right-click menus as OS-native popup menus
//! ([`crate::native_popup_menu`]), where the platform has them.
//!
//! Each `open_native_*` brings its menu up at the pointer and returns `true`;
//! the caller then leaves its drawn menu closed. A choice comes back as a
//! row id and runs the same method the drawn row runs, so the two menus
//! cannot disagree about what an entry does. `false` means the platform has
//! no native menus here and the caller opens the drawn one.

use gpui::{Context, Window};

use super::{plural, CcCurveKind, LaneAction, LaneNoteChoice, PianoRoll, VelocityOperation};
use crate::native_popup_menu::{self, PopupItem};

fn command(label: impl Into<String>, id: impl Into<String>, checked: bool) -> PopupItem {
    PopupItem::Command {
        label: label.into(),
        command: id.into(),
        shortcut: None,
        checked,
        enabled: true,
        icon: None,
    }
}

/// The index after `prefix` in a row id such as `"art:3"`.
fn index_after(id: &str, prefix: &str) -> Option<usize> {
    id.strip_prefix(prefix)?.parse().ok()
}

impl PianoRoll {
    /// Bring up `items` at the pointer; `on_pick` runs on the editor with
    /// the chosen row's id.
    fn show_native_menu(
        &mut self,
        items: Vec<PopupItem>,
        on_pick: impl FnOnce(&mut PianoRoll, &str, &mut Context<PianoRoll>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !native_popup_menu::available() {
            return false;
        }
        let editor = cx.entity().downgrade();
        let position = window.mouse_position();
        native_popup_menu::show(
            window,
            cx,
            position,
            None,
            items,
            Box::new(move |chosen, _window, cx| {
                let Some(chosen) = chosen else {
                    return;
                };
                let _ = editor.update(cx, |editor, cx| {
                    on_pick(editor, &chosen, cx);
                    cx.notify();
                });
            }),
        )
    }

    /// A drum lane's menu: quick pattern actions on that lane's steps.
    /// A rack row's outputs: Main and Out 2 … Out 16, the current one ticked.
    pub(super) fn open_native_output_menu(
        &mut self,
        pad: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let current = self.rack_pad_output(pad);
        let mut items = vec![PopupItem::Header(format!("Pad {} output", pad + 1))];
        items.extend((0..super::sampler_rack::OUTPUTS).map(|output| {
            command(
                super::sampler_rack::output_name(output),
                format!("out:{output}"),
                output == current,
            )
        }));
        self.show_native_menu(
            items,
            move |editor, chosen, cx| {
                if let Some(output) = index_after(chosen, "out:") {
                    editor.set_rack_output(pad, output as u8, cx);
                }
            },
            window,
            cx,
        )
    }

    pub(super) fn open_native_lane_menu(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut items = vec![PopupItem::Header(self.step_row_name(row).to_string())];
        // The drum lanes' note (not the Sampler Rack's, whose pad notes are
        // dragged on the row).
        let lane_note = self
            .drum_rows
            .get(row)
            .copied()
            .zip(self.lane_note_context(cx));
        if let Some((from, context)) = &lane_note {
            let notes = context
                .choices(*from)
                .into_iter()
                .map(|choice| match choice {
                    LaneNoteChoice::Group(group) => PopupItem::Header(group.to_string()),
                    LaneNoteChoice::Note {
                        note,
                        label,
                        checked,
                        enabled,
                    } => PopupItem::Command {
                        label,
                        command: format!("note:{note}"),
                        shortcut: None,
                        checked,
                        enabled,
                        icon: None,
                    },
                })
                .collect();
            items.push(PopupItem::Submenu {
                label: "Lane Note".to_string(),
                enabled: true,
                items: notes,
            });
            items.push(PopupItem::Separator);
        }
        let from = lane_note.map(|(from, _)| from);
        items.extend([
            command("Fill Every 2 Steps", "fill:2", false),
            command("Fill Every 4 Steps", "fill:4", false),
            command("Randomize Velocity (±15%)", "random", false),
            PopupItem::Separator,
            command("Clear Row", "clear", false),
        ]);
        self.show_native_menu(
            items,
            move |editor, chosen, cx| {
                if let (Some(from), Some(to)) = (from, index_after(chosen, "note:")) {
                    if let Ok(to) = u8::try_from(to) {
                        editor.set_lane_note(from, to, cx);
                    }
                    return;
                }
                let action = match chosen {
                    "fill:2" => LaneAction::FillEvery(2),
                    "fill:4" => LaneAction::FillEvery(4),
                    "random" => LaneAction::RandomizeVelocity,
                    "clear" => LaneAction::Clear,
                    _ => return,
                };
                editor.run_lane_action(row, action, cx);
            },
            window,
            cx,
        )
    }

    /// The note menu: articulation (the selection's current one checked),
    /// mute, velocity nudges, delete.
    pub(super) fn open_native_note_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let available = self.available_articulations(cx);
        let current = self.uniform_selection_articulation(cx);
        let selected_count = self.selection.len();
        let all_muted = self.selection_all_muted(cx);

        let mut items = vec![PopupItem::Header("Articulation".to_string())];
        for (index, articulation) in available.iter().enumerate() {
            items.push(command(
                articulation.name(),
                format!("art:{index}"),
                current == Some(Some(*articulation)),
            ));
        }
        items.push(command("None", "art:none", current == Some(None)));
        items.push(PopupItem::Separator);
        items.push(PopupItem::Header("Note".to_string()));
        items.push(command(
            if all_muted { "Unmute" } else { "Mute" },
            "mute",
            all_muted,
        ));
        items.push(command("Velocity +5", "vel:5", false));
        items.push(command("Velocity -5", "vel:-5", false));
        items.push(PopupItem::Separator);
        items.push(command(
            format!("Delete {selected_count} note{}", plural(selected_count)),
            "delete",
            false,
        ));

        self.show_native_menu(
            items,
            move |editor, chosen, cx| match chosen {
                "art:none" => editor.set_selection_articulation(None, cx),
                "mute" => editor.toggle_mute_selection(cx),
                "vel:5" => editor.nudge_selected_velocity(5, cx),
                "vel:-5" => editor.nudge_selected_velocity(-5, cx),
                "delete" => editor.delete_selection(cx),
                other => {
                    if let Some(articulation) =
                        index_after(other, "art:").and_then(|index| available.get(index))
                    {
                        editor.set_selection_articulation(Some(*articulation), cx);
                    }
                }
            },
            window,
            cx,
        )
    }

    /// The velocity lane's menu: one row per velocity operation.
    pub(super) fn open_native_velocity_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut items = vec![PopupItem::Header("Velocity".to_string())];
        for (index, operation) in VelocityOperation::ALL.iter().enumerate() {
            items.push(command(operation.label(), format!("op:{index}"), false));
        }
        self.show_native_menu(
            items,
            |editor, chosen, cx| {
                if let Some(operation) =
                    index_after(chosen, "op:").and_then(|index| VelocityOperation::ALL.get(index))
                {
                    editor.apply_velocity_operation(*operation, cx);
                }
            },
            window,
            cx,
        )
    }

    /// The CC lane's menu: generate a curve from the beat right-clicked.
    pub(super) fn open_native_cc_curve_menu(
        &mut self,
        click_beat: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut items = vec![PopupItem::Header("Generate Curve".to_string())];
        for (index, kind) in CcCurveKind::ALL.iter().enumerate() {
            items.push(command(kind.label(), format!("curve:{index}"), false));
        }
        self.show_native_menu(
            items,
            move |editor, chosen, cx| {
                if let Some(kind) =
                    index_after(chosen, "curve:").and_then(|index| CcCurveKind::ALL.get(index))
                {
                    editor.apply_cc_curve(*kind, click_beat, cx);
                }
            },
            window,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::index_after;

    #[test]
    fn row_ids_carry_their_index() {
        assert_eq!(index_after("art:3", "art:"), Some(3));
        assert_eq!(index_after("art:none", "art:"), None);
        assert_eq!(index_after("op:11", "art:"), None);
    }
}

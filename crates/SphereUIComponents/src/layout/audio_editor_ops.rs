//! Studio side of the Audio Editor: the callbacks it edits the project
//! through, routing of Edit commands to it while it has focus, and the
//! play-selection audition.

use std::rc::Rc;

use gpui::{App, Context, Entity};

use crate::components::{AudioEditorCallbacks, BottomTab};

use super::StudioLayout;

/// Edit commands the Audio Editor takes over while it has keyboard focus.
fn is_audio_editor_edit_command(command_id: &str) -> bool {
    matches!(
        command_id,
        "edit:select-all"
            | "edit:deselect-all"
            | "edit:copy"
            | "edit:cut"
            | "edit:paste"
            | "edit:delete"
            | "edit:delete-backspace"
            | "clip:delete"
            | "clip:erase"
    )
}

impl StudioLayout {
    pub(super) fn audio_editor_callbacks(layout: &Entity<Self>) -> AudioEditorCallbacks {
        let weak = layout.downgrade();
        AudioEditorCallbacks {
            project_folder: {
                let weak = weak.clone();
                Rc::new(move |cx: &App| {
                    weak.upgrade()
                        .and_then(|layout| layout.read(cx).project_folder.clone())
                })
            },
            replace_source: {
                let layout = layout.clone();
                Rc::new(move |clip_id, source, cx: &mut App| {
                    StudioLayout::defer_update(&layout, cx, move |this, cx| {
                        this.replace_clip_source(&clip_id, source, cx);
                        cx.notify();
                    });
                })
            },
            open_tool: {
                let layout = layout.clone();
                Rc::new(move |kind, target, cx: &mut App| {
                    StudioLayout::defer_update(&layout, cx, move |this, cx| {
                        this.open_audio_tool(kind, target, None, cx);
                    });
                })
            },
            play_range: {
                let layout = layout.clone();
                Rc::new(move |start, end, cx: &mut App| {
                    StudioLayout::defer_update(&layout, cx, move |this, cx| {
                        this.play_audio_editor_range(start as f32, end as f32, cx);
                    });
                })
            },
        }
    }

    /// Send an Edit command to the Audio Editor when it owns the keyboard.
    /// Returns whether the editor took it.
    pub(super) fn route_edit_command_to_audio_editor(
        &mut self,
        command_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !is_audio_editor_edit_command(command_id) {
            return false;
        }
        // The editor is only on screen in the docked Editor tab, and not while
        // that tab shows an ARA editor instead. Its focus flag is refreshed
        // when it draws, so off screen it could be stale; the dock check
        // keeps a hidden editor from taking commands meant for the timeline.
        let shown = self.panels.bottom_docked
            && self.active_bottom_tab == BottomTab::Editor
            && !self.clip_editor_panel.read(cx).ara_tab_active();
        if !shown || !self.audio_editor.read(cx).owns_edit_commands() {
            return false;
        }
        let command_id = command_id.to_string();
        self.audio_editor
            .update(cx, |editor, cx| editor.run_command(&command_id, cx))
    }

    /// Play `[start, end)` and stop at `end`, unless playback was already
    /// running (then only jump there).
    fn play_audio_editor_range(&mut self, start: f32, end: f32, cx: &mut Context<Self>) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.seek_to_exact_beat(start, crate::layout::SeekReason::TimelineClick, cx);
        });
        let playing = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|stats| stats.transport_playing)
            .unwrap_or(false);
        if !playing {
            self.start_native_playback(cx);
        }
        self.audio_editor_audition_end = Some(end);
    }

    /// On a transport tick: move the editor's playhead line and end a
    /// play-selection audition once it reaches its end.
    pub(super) fn tick_audio_editor(&mut self, playing: bool, cx: &mut Context<Self>) {
        crate::components::AudioEditorHost::publish_playhead(&self.audio_editor, cx);
        let Some(end) = self.audio_editor_audition_end else {
            return;
        };
        if !playing {
            self.audio_editor_audition_end = None;
            return;
        }
        let playhead = self.timeline.read(cx).state.transport.playhead_beats;
        if playhead >= end {
            self.audio_editor_audition_end = None;
            self.stop_native_playback(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_audio_editor_edit_command;

    #[test]
    fn only_edit_commands_go_to_the_editor() {
        assert!(is_audio_editor_edit_command("edit:cut"));
        assert!(is_audio_editor_edit_command("edit:delete-backspace"));
        assert!(!is_audio_editor_edit_command("edit:undo"));
        assert!(!is_audio_editor_edit_command("transport:play-pause"));
    }
}

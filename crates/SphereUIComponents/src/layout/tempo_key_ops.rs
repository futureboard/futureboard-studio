//! Open the Find Tempo & Key window and apply what it finds.

use std::sync::Arc;

use gpui::{App, Context};

use crate::components::tempo_key_finder::{
    open_tempo_key_finder_window, tempo_key_target, TempoKeyCommand, TempoKeyFinderCallbacks,
};
use crate::components::timeline::timeline_state::{ScaleKind, ScaleRoot};
use crate::components::AudioToolCommand;

use super::StudioLayout;

impl StudioLayout {
    /// Whether the transport's Find Tempo & Key button has a clip to act on.
    pub(super) fn tempo_key_finder_available(&self, cx: &App) -> bool {
        tempo_key_target(&self.timeline.read(cx).state).is_some()
    }

    /// Open (or re-target and raise) the window for the selected audio clip.
    pub(super) fn open_tempo_key_finder(&mut self, cx: &mut Context<Self>) {
        let Some(target) = tempo_key_target(&self.timeline.read(cx).state) else {
            return;
        };
        if let Some(handle) = self.audio_tools.tempo_key {
            let retargeted = handle.update(cx, |finder, window, cx| {
                finder.retarget(target.clone(), cx);
                window.activate_window();
            });
            if retargeted.is_ok() {
                return;
            }
            self.audio_tools.tempo_key = None;
        }

        let layout = cx.entity().clone();
        let callbacks = TempoKeyFinderCallbacks {
            on_command: {
                let layout = layout.clone();
                Arc::new(move |command, cx: &mut App| {
                    StudioLayout::defer_update(&layout, cx, move |this, cx| {
                        this.handle_tempo_key_command(command, cx);
                    });
                })
            },
            on_close: {
                let layout = layout.clone();
                Arc::new(move |bounds, cx: &mut App| {
                    StudioLayout::defer_update(&layout, cx, move |this, _cx| {
                        this.audio_tools.tempo_key_bounds = Some(bounds);
                        this.audio_tools.tempo_key = None;
                    });
                })
            },
        };
        let owner_bounds = self.studio_window_bounds(cx);
        let remembered = self.audio_tools.tempo_key_bounds;
        match open_tempo_key_finder_window(target, owner_bounds, remembered, callbacks, cx) {
            Ok(handle) => self.audio_tools.tempo_key = Some(handle),
            Err(error) => eprintln!("[tempo-key] failed to open window: {error}"),
        }
    }

    fn handle_tempo_key_command(&mut self, command: TempoKeyCommand, cx: &mut Context<Self>) {
        match command {
            TempoKeyCommand::SetProjectTempo { bpm } => {
                // Same target resolution as a typed tempo: with tempo
                // automation the point at the playhead moves, otherwise the
                // project tempo does. One undo entry.
                let target_point_id = {
                    let state = &self.timeline.read(cx).state;
                    if state.tempo_has_automation() {
                        let beat = state.transport.playhead_beats as f64;
                        state
                            .tempo_map
                            .point_id_at_or_before_beat(beat)
                            .map(|id| id.to_string())
                    } else {
                        None
                    }
                };
                let prev = self.capture_tempo_state(cx);
                self.apply_bpm_value(bpm as f32, target_point_id.as_deref(), true, cx);
                self.record_tempo_edit("Set Tempo", prev, cx);
            }
            TempoKeyCommand::SetClipTempo { clip_id, bpm } => {
                // The clip's recorded tempo is the same property the BPM
                // Analyzer's "Use Original BPM" writes.
                self.handle_audio_tool_command(
                    AudioToolCommand::UseOriginalBpm { clip_id, bpm },
                    cx,
                );
            }
            TempoKeyCommand::ApplyScale { tonic, minor } => {
                let root = ScaleRoot::ALL[tonic % 12];
                let kind = if minor {
                    ScaleKind::NaturalMinor
                } else {
                    ScaleKind::Major
                };
                for roll in [self.piano_roll.clone(), self.piano_roll_floating.clone()] {
                    roll.update(cx, |roll, cx| roll.set_scale(root, kind, cx));
                }
            }
        }
        cx.notify();
    }
}

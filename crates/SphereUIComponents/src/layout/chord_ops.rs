//! Chord Track commands and turning chords into MIDI.
//!
//! Everything that changes the project goes through the timeline's edit
//! history: Chord Track edits as one `SetChordEvents` entry, a MIDI clip as one
//! `CreateClip` entry (a new track, like every add-track path in the app, is
//! not itself an undo step).

use std::sync::Arc;

use gpui::{App, Context, Pixels, Point};
use sphere_midi_service::chords::{voice_progression, VoicingOptions};

use crate::components::chord_generator::{
    open_chord_generator_window, ChordGeneratorCallbacks, ChordGeneratorCommand,
};

use crate::components::edit::EditCommand;
use crate::components::timeline::timeline_state::{
    ChordDropTarget, ChordPlacement, ClipType, MidiNoteState, TrackType,
};
use crate::components::timeline::Timeline;

use super::StudioLayout;

/// Velocities for generated chord notes: the bass sits a little forward.
const BASS_VELOCITY: u8 = 96;
const UPPER_VELOCITY: u8 = 84;
/// Gap before the next chord so a repeated pitch re-attacks cleanly.
const NOTE_GAP_BEATS: f32 = 0.02;

impl StudioLayout {
    pub(super) fn edit_chords(
        &mut self,
        label: &'static str,
        edit: impl FnOnce(&mut Timeline),
        cx: &mut Context<Self>,
    ) -> bool {
        let changed = self.timeline.update(cx, |timeline, cx| {
            let prev = timeline.state.chord_events.clone();
            edit(timeline);
            let changed = timeline.record_chord_edit(label, prev, cx);
            cx.notify();
            changed
        });
        if changed {
            self.mark_dirty();
            cx.notify();
        }
        changed
    }

    pub(super) fn set_chord_track_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            if visible {
                timeline.state.show_chord_track_lane();
            } else {
                timeline.state.hide_chord_track_lane();
            }
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn delete_chord_event_command(&mut self, id: u64, cx: &mut Context<Self>) {
        self.edit_chords(
            "Delete Chord",
            |timeline| {
                timeline.state.delete_chord_event(id);
            },
            cx,
        );
    }

    pub(super) fn clear_chord_track_command(&mut self, cx: &mut Context<Self>) {
        self.edit_chords(
            "Delete All Chords",
            |timeline| {
                timeline.state.chord_events.clear();
                timeline.state.selected_chord_event_id = None;
            },
            cx,
        );
    }

    /// Lay a progression onto the Chord Track from `start_beat`, showing the
    /// lane if it was hidden.
    pub(crate) fn place_progression_on_chord_track(
        &mut self,
        start_beat: f64,
        chords: &[ChordPlacement],
        cx: &mut Context<Self>,
    ) {
        if chords.is_empty() {
            return;
        }
        self.set_chord_track_visible(true, cx);
        let chords = chords.to_vec();
        self.edit_chords(
            "Add Chords",
            move |timeline| {
                let ids = timeline.state.place_chords(start_beat, &chords);
                if let Some(first) = ids.first() {
                    timeline.state.select_chord_event(*first);
                }
            },
            cx,
        );
    }

    /// Where "Create MIDI Clip" goes when nothing was dropped: the selected
    /// track if it can play MIDI, else a new MIDI track.
    pub(crate) fn chord_midi_target_for_selection(&self, cx: &gpui::App) -> ChordDropTarget {
        let state = &self.timeline.read(cx).state;
        state
            .selection
            .selected_track_id
            .as_deref()
            .and_then(|id| state.find_track(id))
            .filter(|track| matches!(track.track_type, TrackType::Midi | TrackType::Instrument))
            .map(|track| ChordDropTarget::Track {
                track_id: track.id.clone(),
            })
            .unwrap_or(ChordDropTarget::NewTrack)
    }

    /// Write `chords` as one voiced MIDI clip at `start_beat` on `target`.
    /// Returns the track the clip landed on.
    pub(crate) fn create_progression_midi_clip(
        &mut self,
        target: &ChordDropTarget,
        start_beat: f64,
        chords: &[ChordPlacement],
        voicing: VoicingOptions,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if chords.is_empty() {
            return None;
        }
        let voiced =
            voice_progression(&chords.iter().map(|c| c.chord).collect::<Vec<_>>(), voicing);
        let total: f64 = chords.iter().map(|c| c.length_beats).sum();
        let target = target.clone();
        let track_id = self.timeline.update(cx, |timeline, cx| {
            let track_id = match &target {
                ChordDropTarget::Track { track_id }
                    if timeline.state.find_track(track_id).is_some_and(|t| {
                        matches!(t.track_type, TrackType::Midi | TrackType::Instrument)
                    }) =>
                {
                    track_id.clone()
                }
                _ => timeline.state.create_midi_track(),
            };
            let mut clip = timeline.state.build_midi_clip(
                &track_id,
                start_beat.max(0.0) as f32,
                total as f32,
            )?;
            clip.name = "Chords".to_string();
            let mut notes = Vec::new();
            let mut offset = 0.0_f32;
            for (placement, pitches) in chords.iter().zip(&voiced) {
                let length = (placement.length_beats as f32 - NOTE_GAP_BEATS).max(0.05);
                for (index, &pitch) in pitches.iter().enumerate() {
                    let velocity = if index == 0 && voicing.bass {
                        BASS_VELOCITY
                    } else {
                        UPPER_VELOCITY
                    };
                    notes.push(MidiNoteState::new(pitch, offset, length, velocity));
                }
                offset += placement.length_beats as f32;
            }
            if let ClipType::Midi {
                notes: clip_notes, ..
            } = &mut clip.clip_type
            {
                *clip_notes = notes;
            }
            timeline.state.select_track(&track_id);
            timeline.run_edit_command(
                EditCommand::CreateClip {
                    track_id: track_id.clone(),
                    clip,
                },
                cx,
            );
            cx.notify();
            Some(track_id)
        })?;
        self.mark_dirty();
        self.schedule_audio_project_sync(cx, false, "chord_midi_clip");
        cx.notify();
        Some(track_id)
    }

    /// The whole Chord Track as one MIDI clip on the selected MIDI track (or
    /// a new one), spanning the first chord to the last.
    pub(super) fn chord_track_to_midi_command(&mut self, cx: &mut Context<Self>) {
        let (start, chords) = {
            let state = &self.timeline.read(cx).state;
            let Some(first) = state.chord_events.first() else {
                return;
            };
            let start = first.start_beat;
            // Each chord lasts until the next one starts, so the harmony is
            // held across any gap on the lane rather than dropping to silence.
            let mut chords = Vec::with_capacity(state.chord_events.len());
            for (index, event) in state.chord_events.iter().enumerate() {
                let length = state
                    .chord_events
                    .get(index + 1)
                    .map(|next| next.start_beat - event.start_beat)
                    .unwrap_or(event.length_beats);
                chords.push(ChordPlacement {
                    chord: event.chord,
                    flats: event.flats,
                    length_beats: length,
                });
            }
            (start, chords)
        };
        let target = self.chord_midi_target_for_selection(cx);
        self.create_progression_midi_clip(&target, start, &chords, VoicingOptions::default(), cx);
    }

    /// Open the Chord Generator, or raise it if it is already open.
    pub(super) fn open_chord_generator(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.chord_generator {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.chord_generator = None;
        }
        let layout = cx.entity().downgrade();
        let main_window = self.window_hooks.self_window;
        let callbacks = ChordGeneratorCallbacks {
            on_command: {
                let layout = layout.clone();
                Arc::new(move |command: ChordGeneratorCommand, cx: &mut App| {
                    // Read the arrangement pointer first: the window handle's
                    // update leases the layout, so it cannot happen inside the
                    // layout update below. On macOS this is the live cursor
                    // even while this drag belongs to the generator window.
                    let pointer = match &command {
                        ChordGeneratorCommand::DragMove { .. }
                        | ChordGeneratorCommand::DragEnd { .. } => main_window.and_then(|h| {
                            h.update(cx, |_, window, _| window.mouse_position()).ok()
                        }),
                        _ => None,
                    };
                    layout
                        .update(cx, |this, cx| {
                            this.handle_chord_generator_command(command, pointer, cx)
                        })
                        .ok()
                        .flatten()
                })
            },
            audition: {
                let layout = layout.clone();
                Arc::new(move |notes: &[u8], on: bool, cx: &mut App| {
                    layout
                        .update(cx, |this, cx| this.audition_preview_notes(notes, on, cx))
                        .unwrap_or(false)
                })
            },
            project_bpm: {
                let layout = layout.clone();
                Arc::new(move |cx: &App| {
                    layout
                        .upgrade()
                        .map(|l| {
                            l.read(cx)
                                .timeline
                                .read(cx)
                                .state
                                .effective_bpm_at_playhead() as f32
                        })
                        .unwrap_or(120.0)
                })
            },
            on_close: {
                let layout = layout.clone();
                Arc::new(move |bounds, cx: &mut App| {
                    let _ = layout.update(cx, |this, _| {
                        this.chord_generator_bounds = Some(bounds);
                        this.chord_generator = None;
                    });
                })
            },
        };
        let owner_bounds = self.studio_window_bounds(cx);
        match open_chord_generator_window(owner_bounds, self.chord_generator_bounds, callbacks, cx)
        {
            Ok(handle) => self.chord_generator = Some(handle),
            Err(error) => eprintln!("[chords] failed to open Chord Generator: {error}"),
        }
    }

    fn set_chord_drop_preview(
        &mut self,
        preview: Option<crate::components::timeline::timeline_state::ChordDropPreview>,
        cx: &mut Context<Self>,
    ) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            if timeline.state.chord_drop_preview != preview {
                timeline.state.chord_drop_preview = preview;
                cx.notify();
            }
        });
    }

    fn resolve_chord_pointer(
        &self,
        pointer: Option<Point<Pixels>>,
        cx: &App,
    ) -> Option<(ChordDropTarget, f64)> {
        let pointer = pointer?;
        self.timeline
            .read(cx)
            .state
            .resolve_chord_drop(pointer.x.into(), pointer.y.into())
    }

    fn describe_chord_target(&self, target: &ChordDropTarget, beat: f64, cx: &App) -> String {
        let state = &self.timeline.read(cx).state;
        let at = state.format_position(beat as f32);
        match target {
            ChordDropTarget::ChordTrack => format!("Release to add to the Chord Track at {at}"),
            ChordDropTarget::Track { track_id } => {
                let name = state
                    .find_track(track_id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                format!("Release to write a MIDI clip on {name} at {at}")
            }
            ChordDropTarget::NewTrack => {
                format!("Release to write a MIDI clip on a new track at {at}")
            }
        }
    }

    /// One Chord Generator request. The returned text is the window's footer
    /// status.
    fn handle_chord_generator_command(
        &mut self,
        command: ChordGeneratorCommand,
        pointer: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let playhead = self
            .timeline
            .read(cx)
            .state
            .transport
            .playhead_beats
            .max(0.0) as f64;
        match command {
            ChordGeneratorCommand::PlaceOnChordTrack { chords } => {
                let count = chords.len();
                self.place_progression_on_chord_track(playhead, &chords, cx);
                Some(format!(
                    "Added {count} chords to the Chord Track at the playhead"
                ))
            }
            ChordGeneratorCommand::CreateMidiClip { chords, voicing } => {
                let target = self.chord_midi_target_for_selection(cx);
                let track_id =
                    self.create_progression_midi_clip(&target, playhead, &chords, voicing, cx)?;
                let name = self
                    .timeline
                    .read(cx)
                    .state
                    .find_track(&track_id)
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                Some(format!("Wrote a MIDI clip on {name} at the playhead"))
            }
            ChordGeneratorCommand::DragMove { chords } => {
                match self.resolve_chord_pointer(pointer, cx) {
                    Some((target, beat)) => {
                        let message = self.describe_chord_target(&target, beat, cx);
                        self.set_chord_drop_preview(
                            Some(
                                crate::components::timeline::timeline_state::ChordDropPreview {
                                    target,
                                    start_beat: beat,
                                    chords,
                                },
                            ),
                            cx,
                        );
                        Some(message)
                    }
                    None => {
                        self.set_chord_drop_preview(None, cx);
                        Some("Drop on the Chord Track or a MIDI track".to_string())
                    }
                }
            }
            ChordGeneratorCommand::DragEnd { chords, voicing } => {
                self.set_chord_drop_preview(None, cx);
                let Some((target, beat)) = self.resolve_chord_pointer(pointer, cx) else {
                    return Some("Dropped outside the arrangement — nothing changed".to_string());
                };
                match target {
                    ChordDropTarget::ChordTrack => {
                        let count = chords.len();
                        self.place_progression_on_chord_track(beat, &chords, cx);
                        Some(format!("Added {count} chords to the Chord Track"))
                    }
                    other => {
                        let track_id =
                            self.create_progression_midi_clip(&other, beat, &chords, voicing, cx)?;
                        let name = self
                            .timeline
                            .read(cx)
                            .state
                            .find_track(&track_id)
                            .map(|t| t.name.clone())
                            .unwrap_or_default();
                        Some(format!("Wrote a MIDI clip on {name}"))
                    }
                }
            }
            ChordGeneratorCommand::DragCancel => {
                self.set_chord_drop_preview(None, cx);
                None
            }
        }
    }
}

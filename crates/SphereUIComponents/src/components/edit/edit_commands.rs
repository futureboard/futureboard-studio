//! Undo/redo edit commands — all timeline mutations go through here.

use std::collections::VecDeque;

use crate::components::timeline::timeline_state::{
    AudioClipStretchState, AutomationLaneState, ClipState, GlobalLaneHeights,
    MidiArticulationEvent, MidiControllerKind, MidiControllerPoint, MidiNoteState, SongTextEvent,
    TempoPoint, TimeSignaturePoint, TimelineMarkerState, TimelineRegionState, TimelineState,
    TrackState,
};

/// How a command's effect has to be propagated after execute / undo / redo.
///
/// Every command answers this once, so the four entry points (run, record,
/// undo, redo) route a change the same way. Undo used to only distinguish
/// "metadata or not", which is why undoing a conductor edit left the engine
/// holding the old tempo/meter map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditImpact {
    /// Structural project / audio-graph change.
    Project,
    /// MIDI content change — publish to the scheduler immediately.
    Midi,
    /// Persisted UI metadata; never invalidates the audio graph.
    Metadata,
    /// Live channel-strip value (fader / pan) that the engine already applies
    /// through the realtime `SetTrackVolume` / `SetTrackPan` command path.
    ///
    /// Persisted in the project file, but **not** an audio-graph change: routing
    /// nodes, clips, inserts and plugin instances are all identical before and
    /// after. Classifying it as [`Self::Project`] made every fader release
    /// rebuild the whole engine project — a UI-thread snapshot build, two full
    /// `serde_json` passes over the project for the sync fingerprint, then a
    /// `load_project` that re-decoded clips, rebuilt the graph, re-registered
    /// the plugin bridges and cycled the engine `Running → LoadingProject →
    /// Running` under the playing transport. Mute/solo already took this route.
    MixerControl,
    /// Tempo map or fixed BPM — the engine needs a fresh `set_tempo_map`.
    TempoMap,
    /// Time-signature map — the engine needs a fresh meter map.
    TimeSignatureMap,
}

/// Snapshot of the project's tempo state for undo/redo.
///
/// The Tempo lane edits the marker list and the fixed project BPM together
/// (clearing automation folds the effective BPM back into `bpm`), so one
/// history entry has to carry both halves or undo restores a mismatched pair.
#[derive(Debug, Clone, PartialEq)]
pub struct TempoStateSnapshot {
    pub points: Vec<TempoPoint>,
    pub bpm: f32,
}

impl TempoStateSnapshot {
    pub fn capture(state: &TimelineState) -> Self {
        Self {
            points: state.tempo_map.points.clone(),
            bpm: state.bpm,
        }
    }

    pub fn apply(&self, state: &mut TimelineState) {
        state.restore_tempo_state(self.points.clone(), self.bpm);
    }
}

/// Snapshot of the project's time-signature markers for undo/redo.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSignatureStateSnapshot {
    pub points: Vec<TimeSignaturePoint>,
}

impl TimeSignatureStateSnapshot {
    pub fn capture(state: &TimelineState) -> Self {
        Self {
            points: state.time_signature_map.points.clone(),
        }
    }

    pub fn apply(&self, state: &mut TimelineState) {
        state.restore_time_signature_state(self.points.clone());
    }
}

/// Snapshot of a clip plus its owning track for undo/redo.
#[derive(Debug, Clone)]
pub struct ClipSnapshot {
    pub track_id: String,
    pub clip: ClipState,
}

impl ClipSnapshot {
    pub fn capture(state: &TimelineState, clip_id: &str) -> Option<Self> {
        for track in &state.tracks {
            if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
                return Some(Self {
                    track_id: track.id.clone(),
                    clip: clip.clone(),
                });
            }
        }
        None
    }
}

/// Snapshot of a track plus its original index for undo/redo.
#[derive(Debug, Clone)]
pub struct TrackSnapshot {
    pub index: usize,
    pub track: TrackState,
}

impl TrackSnapshot {
    pub fn capture(state: &TimelineState, track_id: &str) -> Option<Self> {
        state
            .tracks
            .iter()
            .position(|track| track.id == track_id)
            .map(|index| Self {
                index,
                track: state.tracks[index].clone(),
            })
    }
}

/// Editable command with perfect undo.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum EditCommand {
    CreateClip {
        track_id: String,
        clip: ClipState,
    },
    BatchCreateClips {
        clips: Vec<(String, ClipState)>,
    },
    DeleteClip {
        snapshot: ClipSnapshot,
    },
    /// Replaces one existing clip with an exact post-gesture snapshot. Used by
    /// trim and Inspector property gestures so each gesture is one reversible
    /// history entry.
    UpdateClip {
        previous: ClipSnapshot,
        next: ClipSnapshot,
    },
    BatchDeleteClips {
        snapshots: Vec<ClipSnapshot>,
    },
    ReplaceClipWithClips {
        snapshot: ClipSnapshot,
        clips: Vec<(String, ClipState)>,
    },
    DeleteTrack {
        snapshot: TrackSnapshot,
    },
    CreateMidiNote {
        clip_id: String,
        note: MidiNoteState,
    },
    /// Batch note insert (paste / duplicate) — one undo entry for the group.
    CreateMidiNotes {
        clip_id: String,
        notes: Vec<MidiNoteState>,
    },
    DeleteMidiNotes {
        clip_id: String,
        notes: Vec<MidiNoteState>,
    },
    /// Set the muted flag on a set of notes. `prev` snapshots each note's
    /// original muted state so undo restores it exactly.
    SetMidiNotesMuted {
        clip_id: String,
        prev: Vec<(u64, bool)>,
        muted: bool,
    },
    /// In-place transform of a fixed set of notes (move / resize / velocity /
    /// quantize / transpose / nudge). The note set is unchanged — only field
    /// values differ — so `prev`/`next` carry full per-id snapshots and
    /// execute/undo simply overwrite matching notes by id.
    EditMidiNotes {
        clip_id: String,
        prev: Vec<MidiNoteState>,
        next: Vec<MidiNoteState>,
    },
    /// Notes dragged out of one clip and into another, as one undo entry.
    ///
    /// The MIDI editor shows a whole track, so a note can be dragged past the
    /// edge of the clip that owns it. Leaving it there produces a note the
    /// clip's own length says does not exist — stored, not played, and
    /// invisible the moment the editor is closed. It changes owner instead.
    ///
    /// Both clips' complete note lists are carried before and after, rather
    /// than just the notes that moved. Overwriting the destination with only
    /// the arrivals would delete everything already in it, and an undo built
    /// from a diff would have to re-derive what the destination held.
    MoveMidiNotesBetweenClips {
        from_clip_id: String,
        to_clip_id: String,
        from_prev: Vec<MidiNoteState>,
        from_next: Vec<MidiNoteState>,
        to_prev: Vec<MidiNoteState>,
        to_next: Vec<MidiNoteState>,
    },
    /// Replace a track's automation lanes (point add / move / curve / delete,
    /// and lane create / clear / remove / show-hide). One entry per gesture.
    ///
    /// Snapshots the whole `automation_lanes` vec rather than a single lane's
    /// points because drawing the first point *creates* the lane — a
    /// points-only command would leave an empty lane behind on undo.
    SetTrackAutomationLanes {
        track_id: String,
        prev: Vec<AutomationLaneState>,
        next: Vec<AutomationLaneState>,
    },
    /// Replace a controller lane's points (draw / erase gesture). One entry per
    /// gesture; `prev`/`next` are full point snapshots of the lane.
    SetControllerPoints {
        clip_id: String,
        kind: MidiControllerKind,
        prev: Vec<MidiControllerPoint>,
        next: Vec<MidiControllerPoint>,
    },
    /// Replace a clip's direction articulation events (insert / move / delete
    /// gesture). One entry per gesture; `prev`/`next` are full event snapshots
    /// of the clip's articulation lane, mirroring `SetControllerPoints`.
    SetMidiArticulations {
        clip_id: String,
        prev: Vec<MidiArticulationEvent>,
        next: Vec<MidiArticulationEvent>,
    },
    /// Split one note into `parts` (two or more contiguous notes). Atomic so a
    /// single undo restores the original note and removes every part.
    SplitMidiNote {
        clip_id: String,
        original: MidiNoteState,
        parts: Vec<MidiNoteState>,
    },
    /// Set an audio clip's non-destructive stretch/pitch state. `prev`/`next`
    /// snapshot the whole struct so one inspector edit (or one stretch-drag
    /// gesture) is a single, perfectly reversible undo entry. The clip's
    /// timeline length is coupled to the time-stretch ratio (so visual =
    /// audible length, spec §10), hence the duration is snapshotted too.
    SetClipStretch {
        clip_id: String,
        prev: AudioClipStretchState,
        next: AudioClipStretchState,
        prev_duration_beats: f32,
        next_duration_beats: f32,
    },
    /// Reorder a track's FX / insert chain. Stores the full before/after
    /// slot-id order so undo and redo are exact regardless of how the new order
    /// was computed (drag gap math lives in the drop handler, not here). The
    /// operation only reorders the existing slots — it never recreates a plugin
    /// instance — so bypass / enabled / preset / parameter / editor state all
    /// follow each `plugin_instance_id`. One undo entry per completed drag.
    ReorderFxSlot {
        track_id: String,
        before_order: Vec<String>,
        after_order: Vec<String>,
    },
    /// Reorder a track's aux sends. Sends are routed by stable send id, so the
    /// existing send structs move in place and keep gain/enabled/pre-fader state.
    ReorderSendSlot {
        track_id: String,
        before_order: Vec<String>,
        after_order: Vec<String>,
    },
    /// Batch per-track row height changes (layout/view — one undo entry per gesture).
    SetTrackHeights {
        prev: Vec<(String, f32)>,
        next: Vec<(String, f32)>,
    },
    /// One fader gesture (preview → commit). Never reloads the project/graph.
    SetTrackVolume {
        track_id: String,
        prev: f32,
        next: f32,
    },
    /// One pan knob gesture. Never reloads the project/graph.
    SetTrackPan {
        track_id: String,
        prev: f32,
        next: f32,
    },
    SetTrackVolumeAutomationRead {
        track_id: String,
        prev: bool,
        next: bool,
    },
    /// Replace only the affected Song Text events. Empty `previous` creates,
    /// empty `next` deletes, and populated snapshots edit/move atomically.
    SetSongTextEvents {
        label: &'static str,
        previous: Vec<SongTextEvent>,
        next: Vec<SongTextEvent>,
    },
    /// One Tempo-lane gesture or command (add / move / delete / curve / clear /
    /// fixed-BPM edit). Snapshots the whole tempo state rather than a single
    /// marker: adding the first marker *seeds* an anchor at beat 0, clearing
    /// folds the effective BPM back into the fixed `bpm`, and a drag can reorder
    /// the list — none of which a per-point command could reverse exactly.
    SetTempoState {
        label: &'static str,
        prev: TempoStateSnapshot,
        next: TempoStateSnapshot,
    },
    /// One Time Signature-lane gesture or command. Whole-map snapshot for the
    /// same reason as [`EditCommand::SetTempoState`].
    SetTimeSignatureState {
        label: &'static str,
        prev: TimeSignatureStateSnapshot,
        next: TimeSignatureStateSnapshot,
    },
    /// Arrangement marker list (add / import / delete). Markers are a small
    /// sorted list with no per-item identity beyond the id, so the whole vec is
    /// the cheapest exact snapshot.
    SetMarkers {
        label: &'static str,
        prev: Vec<TimelineMarkerState>,
        next: Vec<TimelineMarkerState>,
    },
    /// Arrangement region list (add / delete / one completed drag).
    SetRegions {
        label: &'static str,
        prev: Vec<TimelineRegionState>,
        next: Vec<TimelineRegionState>,
    },
    /// One global-lane height gesture (drag or reset-to-default). Persisted
    /// with the project since v40, but view state all the same, so it never
    /// invalidates the audio graph.
    SetGlobalLaneHeights {
        prev: GlobalLaneHeights,
        next: GlobalLaneHeights,
    },
}

impl EditCommand {
    /// How this command's effect must reach the rest of the app. Answered once
    /// here so execute, record, undo, and redo all propagate it identically.
    pub fn impact(&self) -> EditImpact {
        match self {
            // MIDI content changes are cheap to identify at the command
            // boundary. The owner uses this to publish a note edit immediately
            // instead of waiting for the general 75 ms gesture-sync throttle.
            EditCommand::CreateMidiNote { .. }
            | EditCommand::CreateMidiNotes { .. }
            | EditCommand::DeleteMidiNotes { .. }
            | EditCommand::SetMidiNotesMuted { .. }
            | EditCommand::EditMidiNotes { .. }
            | EditCommand::MoveMidiNotesBetweenClips { .. }
            | EditCommand::SetControllerPoints { .. }
            | EditCommand::SetMidiArticulations { .. }
            | EditCommand::SplitMidiNote { .. } => EditImpact::Midi,
            EditCommand::SetSongTextEvents { .. } => EditImpact::Metadata,
            EditCommand::SetGlobalLaneHeights { .. } => EditImpact::Metadata,
            EditCommand::SetTrackVolume { .. } | EditCommand::SetTrackPan { .. } => {
                EditImpact::MixerControl
            }
            EditCommand::SetTempoState { .. } => EditImpact::TempoMap,
            EditCommand::SetTimeSignatureState { .. } => EditImpact::TimeSignatureMap,
            _ => EditImpact::Project,
        }
    }

    pub fn is_midi_edit(&self) -> bool {
        self.impact() == EditImpact::Midi
    }

    pub fn is_metadata_only(&self) -> bool {
        self.impact() == EditImpact::Metadata
    }

    /// `(track_id, engine param id)` for an [`EditImpact::MixerControl`] command.
    ///
    /// Every UI gesture that produces one of these already pushes the value down
    /// the realtime control path itself; undo/redo do not, because they only
    /// mutate `TimelineState`. Since these commands no longer dirty the engine
    /// graph, undo/redo have to make that push instead of leaning on the next
    /// project sync to carry the value across.
    pub fn mixer_control_target(&self) -> Option<(&str, &'static str)> {
        match self {
            EditCommand::SetTrackVolume { track_id, .. } => Some((track_id.as_str(), "volume")),
            EditCommand::SetTrackPan { track_id, .. } => Some((track_id.as_str(), "pan")),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            EditCommand::CreateClip { .. } => "Create Clip",
            EditCommand::BatchCreateClips { .. } => "Create Clips",
            EditCommand::DeleteClip { .. } => "Delete Clip",
            EditCommand::UpdateClip { .. } => "Edit Clip",
            EditCommand::BatchDeleteClips { .. } => "Delete Clips",
            EditCommand::ReplaceClipWithClips { .. } => "Split Clip",
            EditCommand::DeleteTrack { .. } => "Delete Track",
            EditCommand::CreateMidiNote { .. } => "Create MIDI Note",
            EditCommand::CreateMidiNotes { .. } => "Add MIDI Notes",
            EditCommand::DeleteMidiNotes { .. } => "Delete MIDI Notes",
            EditCommand::SetMidiNotesMuted { muted, .. } => {
                if *muted {
                    "Mute Notes"
                } else {
                    "Unmute Notes"
                }
            }
            EditCommand::EditMidiNotes { .. } => "Edit MIDI Notes",
            EditCommand::MoveMidiNotesBetweenClips { .. } => "Move MIDI Notes to Clip",
            EditCommand::SetTrackAutomationLanes { .. } => "Edit Automation",
            EditCommand::SetControllerPoints { .. } => "Edit CC Lane",
            EditCommand::SetMidiArticulations { .. } => "Edit Articulations",
            EditCommand::SplitMidiNote { .. } => "Split MIDI Note",
            EditCommand::SetClipStretch { .. } => "Edit Stretch",
            EditCommand::ReorderFxSlot { .. } => "Reorder FX",
            EditCommand::ReorderSendSlot { .. } => "Reorder Sends",
            EditCommand::SetTrackHeights { .. } => "Resize Track Height",
            EditCommand::SetTrackVolume { .. } => "Set Volume",
            EditCommand::SetTrackPan { .. } => "Set Pan",
            EditCommand::SetTrackVolumeAutomationRead { .. } => "Set Volume Automation Read",
            EditCommand::SetSongTextEvents { label, .. } => label,
            EditCommand::SetTempoState { label, .. } => label,
            EditCommand::SetTimeSignatureState { label, .. } => label,
            EditCommand::SetMarkers { label, .. } => label,
            EditCommand::SetRegions { label, .. } => label,
            EditCommand::SetGlobalLaneHeights { .. } => "Resize Lane",
        }
    }

    pub fn execute(&self, state: &mut TimelineState) {
        match self {
            EditCommand::CreateClip { track_id, clip } => {
                if let Some(track) = state.tracks.iter_mut().find(|t| t.id == *track_id) {
                    track.clips.push(clip.clone());
                    state.selection.selected_track_id = Some(track_id.clone());
                    state.selection.selected_clip_ids = vec![clip.id.clone()];
                }
            }
            EditCommand::BatchCreateClips { clips } => {
                let mut selected = Vec::new();
                let mut selected_track = None;
                for (track_id, clip) in clips {
                    if let Some(track) = state.tracks.iter_mut().find(|t| t.id == *track_id) {
                        track.clips.push(clip.clone());
                        selected_track = Some(track_id.clone());
                        selected.push(clip.id.clone());
                    }
                }
                if !selected.is_empty() {
                    state.selection.selected_track_id = selected_track;
                    state.selection.selected_clip_ids = selected;
                }
            }
            EditCommand::DeleteClip { snapshot } => {
                state.delete_clip(&snapshot.clip.id);
            }
            EditCommand::UpdateClip { previous, next } => {
                state.delete_clip(&previous.clip.id);
                restore_clip_snapshot(state, next);
            }
            EditCommand::BatchDeleteClips { snapshots } => {
                for snap in snapshots {
                    state.delete_clip(&snap.clip.id);
                }
            }
            EditCommand::ReplaceClipWithClips { snapshot, clips } => {
                state.delete_clip(&snapshot.clip.id);
                for (track_id, clip) in clips {
                    if let Some(track) = state.tracks.iter_mut().find(|t| t.id == *track_id) {
                        if !track.clips.iter().any(|c| c.id == clip.id) {
                            track.clips.push(clip.clone());
                        }
                    }
                }
                state.selection.selected_track_id = Some(snapshot.track_id.clone());
                state.selection.selected_clip_ids =
                    clips.iter().map(|(_, clip)| clip.id.clone()).collect();
            }
            EditCommand::DeleteTrack { snapshot } => {
                state.delete_track(&snapshot.track.id);
            }
            EditCommand::CreateMidiNote { clip_id, note } => {
                if let Some(notes) = state.midi_clip_notes_mut(clip_id) {
                    if !notes.iter().any(|n| n.id == note.id) {
                        notes.push(note.clone());
                    }
                }
                // A note drawn past the clip end auto-expands the clip so it is
                // always contained. Applies to redo too.
                state.expand_clip_to_contain_notes(clip_id);
            }
            EditCommand::CreateMidiNotes { clip_id, notes } => {
                if let Some(existing) = state.midi_clip_notes_mut(clip_id) {
                    for note in notes {
                        if !existing.iter().any(|n| n.id == note.id) {
                            existing.push(note.clone());
                        }
                    }
                }
                state.expand_clip_to_contain_notes(clip_id);
            }
            EditCommand::DeleteMidiNotes { clip_id, notes } => {
                let ids: Vec<u64> = notes.iter().map(|n| n.id).collect();
                state.delete_midi_notes(clip_id, &ids);
            }
            EditCommand::SetMidiNotesMuted {
                clip_id,
                prev,
                muted,
            } => {
                let ids: Vec<u64> = prev.iter().map(|(id, _)| *id).collect();
                state.set_midi_notes_muted(clip_id, &ids, *muted);
            }
            EditCommand::EditMidiNotes { clip_id, next, .. } => {
                state.overwrite_midi_notes(clip_id, next);
            }
            EditCommand::MoveMidiNotesBetweenClips {
                from_clip_id,
                to_clip_id,
                from_next,
                to_next,
                ..
            } => {
                state.overwrite_midi_notes(from_clip_id, from_next);
                state.overwrite_midi_notes(to_clip_id, to_next);
            }
            EditCommand::SetTrackAutomationLanes { track_id, next, .. } => {
                state.set_track_automation_lanes(track_id, next.clone());
            }
            EditCommand::SetControllerPoints {
                clip_id,
                kind,
                next,
                ..
            } => {
                state.set_controller_lane_points(clip_id, *kind, next.clone());
            }
            EditCommand::SetMidiArticulations { clip_id, next, .. } => {
                state.set_midi_articulations(clip_id, next.clone());
            }
            EditCommand::SplitMidiNote {
                clip_id,
                original,
                parts,
            } => {
                state.delete_midi_notes(clip_id, &[original.id]);
                if let Some(existing) = state.midi_clip_notes_mut(clip_id) {
                    for note in parts {
                        if !existing.iter().any(|n| n.id == note.id) {
                            existing.push(note.clone());
                        }
                    }
                }
                state.expand_clip_to_contain_notes(clip_id);
            }
            EditCommand::SetClipStretch {
                clip_id,
                next,
                next_duration_beats,
                ..
            } => {
                state.set_clip_stretch(clip_id, next.clone());
                state.set_clip_length(clip_id, *next_duration_beats);
            }
            EditCommand::ReorderFxSlot {
                track_id,
                after_order,
                ..
            } => {
                state.set_insert_order(track_id, after_order);
            }
            EditCommand::ReorderSendSlot {
                track_id,
                after_order,
                ..
            } => {
                state.set_send_order(track_id, after_order);
            }
            EditCommand::SetTrackHeights { next, .. } => {
                apply_track_heights_snapshot(state, next);
            }
            EditCommand::SetTrackVolume { track_id, next, .. } => {
                state.set_track_volume(track_id, *next);
            }
            EditCommand::SetTrackPan { track_id, next, .. } => {
                state.set_track_pan(track_id, *next);
            }
            EditCommand::SetTrackVolumeAutomationRead { track_id, next, .. } => {
                let beat = state.transport.playhead_beats;
                state.set_track_volume_automation_read(track_id, *next);
                state.recompute_effective_volumes(beat, "automation_read_edit");
            }
            EditCommand::SetSongTextEvents { previous, next, .. } => {
                apply_song_text_snapshot(state, previous, next);
            }
            EditCommand::SetTempoState { next, .. } => next.apply(state),
            EditCommand::SetTimeSignatureState { next, .. } => next.apply(state),
            EditCommand::SetMarkers { next, .. } => {
                state.markers = next.clone();
            }
            EditCommand::SetRegions { next, .. } => {
                state.regions = next.clone();
            }
            EditCommand::SetGlobalLaneHeights { next, .. } => {
                state.global_lane_heights = next.clone();
            }
        }
    }

    pub fn undo(&self, state: &mut TimelineState) {
        match self {
            EditCommand::CreateClip { clip, .. } => {
                state.delete_clip(&clip.id);
            }
            EditCommand::BatchCreateClips { clips } => {
                for (_, clip) in clips {
                    state.delete_clip(&clip.id);
                }
            }
            EditCommand::DeleteClip { snapshot } => {
                restore_clip_snapshot(state, snapshot);
            }
            EditCommand::UpdateClip { previous, next } => {
                state.delete_clip(&next.clip.id);
                restore_clip_snapshot(state, previous);
            }
            EditCommand::BatchDeleteClips { snapshots } => {
                for snap in snapshots {
                    restore_clip_snapshot(state, snap);
                }
            }
            EditCommand::ReplaceClipWithClips { snapshot, clips } => {
                for (_, clip) in clips {
                    state.delete_clip(&clip.id);
                }
                restore_clip_snapshot(state, snapshot);
                state.selection.selected_track_id = Some(snapshot.track_id.clone());
                state.selection.selected_clip_ids = vec![snapshot.clip.id.clone()];
            }
            EditCommand::DeleteTrack { snapshot } => {
                restore_track_snapshot(state, snapshot);
            }
            EditCommand::CreateMidiNote { clip_id, note } => {
                state.delete_midi_notes(clip_id, &[note.id]);
            }
            EditCommand::CreateMidiNotes { clip_id, notes } => {
                let ids: Vec<u64> = notes.iter().map(|n| n.id).collect();
                state.delete_midi_notes(clip_id, &ids);
            }
            EditCommand::DeleteMidiNotes { clip_id, notes } => {
                if let Some(existing) = state.midi_clip_notes_mut(clip_id) {
                    for note in notes {
                        if !existing.iter().any(|n| n.id == note.id) {
                            existing.push(note.clone());
                        }
                    }
                }
            }
            EditCommand::SetMidiNotesMuted { clip_id, prev, .. } => {
                if let Some(existing) = state.midi_clip_notes_mut(clip_id) {
                    for (id, was) in prev {
                        if let Some(note) = existing.iter_mut().find(|n| n.id == *id) {
                            note.muted = *was;
                        }
                    }
                }
            }
            EditCommand::EditMidiNotes { clip_id, prev, .. } => {
                state.overwrite_midi_notes(clip_id, prev);
            }
            EditCommand::MoveMidiNotesBetweenClips {
                from_clip_id,
                to_clip_id,
                from_prev,
                to_prev,
                ..
            } => {
                state.overwrite_midi_notes(from_clip_id, from_prev);
                state.overwrite_midi_notes(to_clip_id, to_prev);
            }
            EditCommand::SetTrackAutomationLanes { track_id, prev, .. } => {
                state.set_track_automation_lanes(track_id, prev.clone());
            }
            EditCommand::SetControllerPoints {
                clip_id,
                kind,
                prev,
                ..
            } => {
                state.set_controller_lane_points(clip_id, *kind, prev.clone());
            }
            EditCommand::SetMidiArticulations { clip_id, prev, .. } => {
                state.set_midi_articulations(clip_id, prev.clone());
            }
            EditCommand::SplitMidiNote {
                clip_id,
                original,
                parts,
            } => {
                let ids: Vec<u64> = parts.iter().map(|n| n.id).collect();
                state.delete_midi_notes(clip_id, &ids);
                if let Some(existing) = state.midi_clip_notes_mut(clip_id) {
                    if !existing.iter().any(|n| n.id == original.id) {
                        existing.push(original.clone());
                    }
                }
                state.expand_clip_to_contain_notes(clip_id);
            }
            EditCommand::SetClipStretch {
                clip_id,
                prev,
                prev_duration_beats,
                ..
            } => {
                state.set_clip_stretch(clip_id, prev.clone());
                state.set_clip_length(clip_id, *prev_duration_beats);
            }
            EditCommand::ReorderFxSlot {
                track_id,
                before_order,
                ..
            } => {
                state.set_insert_order(track_id, before_order);
            }
            EditCommand::ReorderSendSlot {
                track_id,
                before_order,
                ..
            } => {
                state.set_send_order(track_id, before_order);
            }
            EditCommand::SetTrackHeights { prev, .. } => {
                apply_track_heights_snapshot(state, prev);
            }
            EditCommand::SetTrackVolume { track_id, prev, .. } => {
                state.set_track_volume(track_id, *prev);
            }
            EditCommand::SetTrackPan { track_id, prev, .. } => {
                state.set_track_pan(track_id, *prev);
            }
            EditCommand::SetTrackVolumeAutomationRead { track_id, prev, .. } => {
                let beat = state.transport.playhead_beats;
                state.set_track_volume_automation_read(track_id, *prev);
                state.recompute_effective_volumes(beat, "automation_read_edit");
            }
            EditCommand::SetSongTextEvents { previous, next, .. } => {
                apply_song_text_snapshot(state, next, previous);
            }
            EditCommand::SetTempoState { prev, .. } => prev.apply(state),
            EditCommand::SetTimeSignatureState { prev, .. } => prev.apply(state),
            EditCommand::SetMarkers { prev, .. } => {
                state.markers = prev.clone();
            }
            EditCommand::SetRegions { prev, .. } => {
                state.regions = prev.clone();
            }
            EditCommand::SetGlobalLaneHeights { prev, .. } => {
                state.global_lane_heights = prev.clone();
            }
        }
    }
}

fn apply_song_text_snapshot(
    state: &mut TimelineState,
    remove: &[SongTextEvent],
    insert: &[SongTextEvent],
) {
    state.apply_song_text_patch(remove, insert);
}

fn apply_track_heights_snapshot(state: &mut TimelineState, heights: &[(String, f32)]) {
    for (track_id, height) in heights {
        if (*height - crate::components::timeline::timeline_state::DEFAULT_TRACK_HEIGHT).abs()
            < 0.01
        {
            state.track_view_layout.remove_track(track_id);
        } else if state.tracks.iter().any(|t| t.id == *track_id) {
            state
                .track_view_layout
                .set_height(track_id.clone(), *height);
        }
    }
}

fn restore_clip_snapshot(state: &mut TimelineState, snapshot: &ClipSnapshot) {
    if let Some(track) = state.tracks.iter_mut().find(|t| t.id == snapshot.track_id) {
        if !track.clips.iter().any(|c| c.id == snapshot.clip.id) {
            track.clips.push(snapshot.clip.clone());
        }
    }
}

#[cfg(test)]
mod mixer_control_impact_tests {
    use super::*;

    /// Fader and pan are live channel-strip values, not graph structure.
    ///
    /// [`EditImpact::Project`] routes to `mark_project_changed`, which dirties
    /// the engine and makes the next poll build a full project snapshot, JSON-
    /// serialize it twice for the sync fingerprint and run `load_project` —
    /// rebuilding the graph, re-decoding clips and re-registering the plug-in
    /// bridges under a playing transport, once per fader release. The value
    /// itself already reached the audio thread through `SetTrackVolume` /
    /// `SetTrackPan` before the command was ever recorded.
    #[test]
    fn fader_and_pan_do_not_dirty_the_audio_graph() {
        let volume = EditCommand::SetTrackVolume {
            track_id: "track-1".to_string(),
            prev: 0.8,
            next: 0.2,
        };
        let pan = EditCommand::SetTrackPan {
            track_id: "track-1".to_string(),
            prev: 0.0,
            next: -0.5,
        };
        assert_eq!(volume.impact(), EditImpact::MixerControl);
        assert_eq!(pan.impact(), EditImpact::MixerControl);
        assert_eq!(volume.mixer_control_target(), Some(("track-1", "volume")));
        assert_eq!(pan.mixer_control_target(), Some(("track-1", "pan")));
    }

    /// A structural edit must not be swept into the live-control path with them.
    #[test]
    fn structural_edits_still_report_project_impact() {
        let heights = EditCommand::SetTrackHeights {
            prev: Vec::new(),
            next: Vec::new(),
        };
        assert_eq!(heights.impact(), EditImpact::Project);
        assert_eq!(heights.mixer_control_target(), None);
    }

    /// Undo/redo push the restored value themselves now that the sync no longer
    /// carries it, and they find the applied command on the opposite stack.
    #[test]
    fn history_exposes_the_command_undo_and_redo_just_applied() {
        let mut state = TimelineState::default();
        let track_id = "track-1".to_string();
        let mut history = EditHistory::new(10);
        let command = EditCommand::SetTrackVolume {
            track_id: track_id.clone(),
            prev: 0.8,
            next: 0.2,
        };
        command.execute(&mut state);
        history.push(command);

        assert_eq!(
            history.undo_with_impact(&mut state),
            Some(EditImpact::MixerControl)
        );
        assert_eq!(
            history
                .last_undone()
                .and_then(EditCommand::mixer_control_target),
            Some((track_id.as_str(), "volume"))
        );

        assert_eq!(
            history.redo_with_impact(&mut state),
            Some(EditImpact::MixerControl)
        );
        assert_eq!(
            history
                .last_redone()
                .and_then(EditCommand::mixer_control_target),
            Some((track_id.as_str(), "volume"))
        );
    }
}

#[cfg(test)]
mod song_text_command_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{SongTextEvent, SongTextEventType};

    #[test]
    fn chord_and_lyric_pair_is_one_undoable_command() {
        let mut state = TimelineState::default();
        let chord = SongTextEvent::chord(4.0, "Am7").unwrap();
        let lyric = SongTextEvent::lyric(4.0, "I remember").unwrap();
        let command = EditCommand::SetSongTextEvents {
            label: "Add Chord and Lyric",
            previous: Vec::new(),
            next: vec![chord.clone(), lyric.clone()],
        };
        let mut history = EditHistory::new(10);
        command.execute(&mut state);
        history.push(command);
        assert_eq!(state.song_text_events.len(), 2);
        assert!(history.undo(&mut state));
        assert!(state.song_text_events.is_empty());
        assert!(
            !history.undo(&mut state),
            "the pair must consume one history item"
        );
        assert!(history.redo(&mut state));
        assert_eq!(
            state
                .song_text_events
                .iter()
                .map(SongTextEvent::event_type)
                .collect::<Vec<_>>(),
            vec![SongTextEventType::Chord, SongTextEventType::Lyric]
        );
    }

    #[test]
    fn text_edit_undo_restores_exact_content_and_id() {
        let mut state = TimelineState::default();
        let previous = SongTextEvent::lyric(2.5, "before").unwrap();
        state.upsert_song_text_event(previous.clone());
        let mut next = previous.clone();
        if let crate::components::timeline::timeline_state::SongTextEventKind::Lyric(lyric) =
            &mut next.kind
        {
            lyric.text = "after".to_string();
        }
        let command = EditCommand::SetSongTextEvents {
            label: "Edit Song Text",
            previous: vec![previous.clone()],
            next: vec![next.clone()],
        };
        command.execute(&mut state);
        assert_eq!(state.song_text_event(&previous.id), Some(&next));
        command.undo(&mut state);
        assert_eq!(state.song_text_event(&previous.id), Some(&previous));
    }
}

#[cfg(test)]
mod inspector_gesture_command_tests {
    use super::*;

    #[test]
    fn pan_preview_commits_as_one_undoable_command() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_audio_track();
        state.begin_track_pan_preview(&track_id);
        assert!(state.set_track_pan_preview(&track_id, 0.25));
        assert!(state.set_track_pan_preview(&track_id, 0.6));
        let (prev, next) = state
            .commit_track_pan_preview(&track_id)
            .expect("pan gesture");

        let mut history = EditHistory::new(8);
        history.push(EditCommand::SetTrackPan {
            track_id: track_id.clone(),
            prev,
            next,
        });
        assert!((state.find_track(&track_id).unwrap().pan - 0.6).abs() < 1.0e-6);
        assert!(history.undo(&mut state));
        assert!((state.find_track(&track_id).unwrap().pan - prev).abs() < 1.0e-6);
        assert!(!history.undo(&mut state), "one drag must create one entry");
        assert!(history.redo(&mut state));
        assert!((state.find_track(&track_id).unwrap().pan - 0.6).abs() < 1.0e-6);
    }

    /// Set up a track with one volume automation lane holding `beats` points.
    fn track_with_automation(beats: &[f32]) -> (TimelineState, String, String) {
        use crate::components::timeline::timeline_state::AutomationTarget;
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_audio_track();
        let lane_id = state
            .ensure_automation_lane(&track_id, AutomationTarget::TrackVolume)
            .expect("lane");
        for beat in beats {
            state.add_automation_point(&track_id, &lane_id, *beat, 0.5);
        }
        (state, track_id, lane_id)
    }

    fn point_count(state: &TimelineState, track_id: &str, lane_id: &str) -> usize {
        state
            .automation_lane(track_id, lane_id)
            .map(|lane| lane.points.len())
            .unwrap_or(0)
    }

    #[test]
    fn automation_point_edits_are_undoable_and_redoable() {
        let (mut state, track_id, lane_id) = track_with_automation(&[0.0, 1.0]);
        let prev = state.capture_automation_lanes(&track_id);
        assert_eq!(point_count(&state, &track_id, &lane_id), 2);

        // A drag-style edit: mutate live, then record the result.
        state.add_automation_point(&track_id, &lane_id, 2.0, 0.9);
        let next = state.capture_automation_lanes(&track_id);
        assert_eq!(point_count(&state, &track_id, &lane_id), 3);

        let mut history = EditHistory::new(8);
        history.push(EditCommand::SetTrackAutomationLanes {
            track_id: track_id.clone(),
            prev,
            next,
        });

        assert!(history.undo(&mut state));
        assert_eq!(point_count(&state, &track_id, &lane_id), 2);
        assert!(!history.undo(&mut state), "one gesture is one entry");
        assert!(history.redo(&mut state));
        assert_eq!(point_count(&state, &track_id, &lane_id), 3);
    }

    #[test]
    fn undoing_the_first_point_removes_the_lane_it_created() {
        use crate::components::timeline::timeline_state::AutomationTarget;
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_audio_track();
        // Baseline captured before the lane exists — this is the case a
        // points-only command could not express.
        let prev = state.capture_automation_lanes(&track_id);
        assert!(prev.is_empty());

        let lane_id = state
            .ensure_automation_lane(&track_id, AutomationTarget::TrackVolume)
            .expect("lane");
        state.add_automation_point(&track_id, &lane_id, 0.0, 0.75);
        let next = state.capture_automation_lanes(&track_id);

        let mut history = EditHistory::new(8);
        history.push(EditCommand::SetTrackAutomationLanes {
            track_id: track_id.clone(),
            prev,
            next,
        });
        assert!(history.undo(&mut state));
        assert!(
            state.capture_automation_lanes(&track_id).is_empty(),
            "undo must remove the lane, not leave it empty"
        );
        assert!(history.redo(&mut state));
        assert_eq!(point_count(&state, &track_id, &lane_id), 1);
    }

    #[test]
    fn clearing_and_removing_lanes_is_undoable() {
        let (mut state, track_id, lane_id) = track_with_automation(&[0.0, 1.0, 2.0]);
        let mut history = EditHistory::new(8);

        let prev = state.capture_automation_lanes(&track_id);
        assert!(state.clear_automation_lane(&track_id, &lane_id) > 0);
        history.push(EditCommand::SetTrackAutomationLanes {
            track_id: track_id.clone(),
            prev,
            next: state.capture_automation_lanes(&track_id),
        });
        assert_eq!(point_count(&state, &track_id, &lane_id), 0);
        assert!(history.undo(&mut state));
        assert_eq!(point_count(&state, &track_id, &lane_id), 3);

        let prev = state.capture_automation_lanes(&track_id);
        assert!(state.remove_automation_lane(&track_id, &lane_id));
        history.push(EditCommand::SetTrackAutomationLanes {
            track_id: track_id.clone(),
            prev,
            next: state.capture_automation_lanes(&track_id),
        });
        assert!(state.automation_lane(&track_id, &lane_id).is_none());
        assert!(history.undo(&mut state));
        assert_eq!(point_count(&state, &track_id, &lane_id), 3);
    }

    #[test]
    fn restoring_lanes_keeps_the_selected_target_valid() {
        use crate::components::timeline::timeline_state::AutomationTarget;
        let (mut state, track_id, lane_id) = track_with_automation(&[0.0]);
        state.set_track_automation_target(&track_id, AutomationTarget::TrackVolume);

        let prev = state.capture_automation_lanes(&track_id);
        assert!(state.remove_automation_lane(&track_id, &lane_id));
        // The selected target pointed at the lane that just went away; it must
        // not be left dangling after a restore either.
        state.set_track_automation_lanes(&track_id, prev);
        let track = state.find_track(&track_id).unwrap();
        assert!(track
            .selected_automation_target
            .as_ref()
            .is_some_and(|t| track.automation_lanes.iter().any(|l| l.target == *t)));
    }

    #[test]
    fn volume_automation_read_is_undoable() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_audio_track();
        let command = EditCommand::SetTrackVolumeAutomationRead {
            track_id: track_id.clone(),
            prev: true,
            next: false,
        };
        let mut history = EditHistory::new(8);
        command.execute(&mut state);
        history.push(command);
        assert!(!state.find_track(&track_id).unwrap().volume_automation_read);
        assert!(history.undo(&mut state));
        assert!(state.find_track(&track_id).unwrap().volume_automation_read);
        assert!(history.redo(&mut state));
        assert!(!state.find_track(&track_id).unwrap().volume_automation_read);
    }
}

#[cfg(test)]
mod stretch_command_tests {
    use super::*;
    use crate::components::timeline::timeline_state::StretchMode;

    #[test]
    fn set_clip_stretch_executes_undoes_and_redoes() {
        let mut state = TimelineState::demo_project();
        let prev = state
            .clip_stretch("clip-1")
            .cloned()
            .expect("demo audio clip-1");
        let prev_len = state.clip_duration_beats("clip-1").expect("clip-1 length");
        let mut next = prev.clone();
        next.mode = StretchMode::Manual;
        next.set_stretch_ratio(1.5);
        let next_len = prev_len * 1.5; // length follows the ratio (spec §10)
        assert_ne!(prev, next, "test setup must produce a real change");

        let cmd = EditCommand::SetClipStretch {
            clip_id: "clip-1".to_string(),
            prev: prev.clone(),
            next: next.clone(),
            prev_duration_beats: prev_len,
            next_duration_beats: next_len,
        };
        assert_eq!(cmd.label(), "Edit Stretch");

        cmd.execute(&mut state);
        assert_eq!(state.clip_stretch("clip-1"), Some(&next));
        assert!((state.clip_duration_beats("clip-1").unwrap() - next_len).abs() < 0.001);

        cmd.undo(&mut state);
        assert_eq!(state.clip_stretch("clip-1"), Some(&prev));
        assert!((state.clip_duration_beats("clip-1").unwrap() - prev_len).abs() < 0.001);

        // Redo re-applies `next` and its coupled length.
        cmd.execute(&mut state);
        assert_eq!(state.clip_stretch("clip-1"), Some(&next));
        assert!((state.clip_duration_beats("clip-1").unwrap() - next_len).abs() < 0.001);
    }

    #[test]
    fn edit_history_keeps_the_newest_bounded_steps() {
        let mut history = EditHistory::new(2);
        let command = || EditCommand::SetTrackHeights {
            prev: Vec::new(),
            next: Vec::new(),
        };
        history.push(command());
        history.push(command());
        history.push(command());

        let mut state = TimelineState::default();
        assert!(history.undo(&mut state));
        assert!(history.undo(&mut state));
        assert!(!history.undo(&mut state), "oldest entry must be evicted");
        assert!(history.redo(&mut state));
        assert!(history.redo(&mut state));
        assert!(!history.redo(&mut state));
    }
}

#[cfg(test)]
mod articulation_command_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{ArticulationId, MidiArticulationEvent};

    fn midi_state() -> (TimelineState, String) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_midi_track();
        let clip_id = state.create_midi_clip(&track_id, 0.0, 8.0).expect("clip");
        (state, clip_id)
    }

    #[test]
    fn set_midi_articulations_executes_undoes_and_redoes() {
        let (mut state, clip_id) = midi_state();
        let prev = state.articulations_snapshot(&clip_id);
        assert!(prev.is_empty());
        let next = vec![
            MidiArticulationEvent::new(0.0, ArticulationId::Sustain),
            MidiArticulationEvent::new(4.0, ArticulationId::Legato),
        ];
        let cmd = EditCommand::SetMidiArticulations {
            clip_id: clip_id.clone(),
            prev: prev.clone(),
            next: next.clone(),
        };
        assert_eq!(cmd.label(), "Edit Articulations");

        cmd.execute(&mut state);
        assert_eq!(state.articulations_snapshot(&clip_id), next);
        cmd.undo(&mut state);
        assert!(state.articulations_snapshot(&clip_id).is_empty());
        cmd.execute(&mut state);
        assert_eq!(state.articulations_snapshot(&clip_id), next);
    }

    #[test]
    fn edit_midi_notes_round_trips_per_note_articulation() {
        let (mut state, clip_id) = midi_state();
        let id = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        let prev = vec![state.midi_clip_notes(&clip_id).unwrap()[0].clone()];
        let mut next = prev.clone();
        next[0].articulation = Some(ArticulationId::Staccato);

        let cmd = EditCommand::EditMidiNotes {
            clip_id: clip_id.clone(),
            prev,
            next,
        };
        cmd.execute(&mut state);
        let articulation_of = |state: &TimelineState| {
            state
                .midi_clip_notes(&clip_id)
                .unwrap()
                .iter()
                .find(|n| n.id == id)
                .unwrap()
                .articulation
        };
        assert_eq!(articulation_of(&state), Some(ArticulationId::Staccato));
        cmd.undo(&mut state);
        assert_eq!(articulation_of(&state), None);
        cmd.execute(&mut state);
        assert_eq!(articulation_of(&state), Some(ArticulationId::Staccato));
    }
}

/// The conductor lanes (tempo, meter, markers, regions) used to mutate the
/// project outside the command system entirely, so none of them could be undone
/// or redone. These guard the round trip *and* the impact routing, because an
/// undone tempo edit that does not report `TempoMap` leaves the engine holding
/// the old map even though the UI shows the old value.
#[cfg(test)]
mod conductor_command_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        GlobalLaneHeights, GlobalLaneKind, TempoCurve, TimelineMarkerState, TimelineRegionState,
    };

    fn tempo_command(state: &mut TimelineState, label: &'static str) -> EditCommand {
        let prev = TempoStateSnapshot::capture(state);
        state
            .tempo_map
            .add_or_update_point(8.0, 150.0, TempoCurve::Hold);
        let next = TempoStateSnapshot::capture(state);
        EditCommand::SetTempoState { label, prev, next }
    }

    #[test]
    fn tempo_marker_add_undoes_and_redoes() {
        let mut state = TimelineState::default();
        let mut history = EditHistory::new(8);
        let cmd = tempo_command(&mut state, "Add Tempo Marker");
        history.push(cmd);
        assert_eq!(state.tempo_map.points.len(), 1);

        assert_eq!(
            history.undo_with_impact(&mut state),
            Some(EditImpact::TempoMap)
        );
        assert!(
            state.tempo_map.points.is_empty(),
            "undo must remove the marker"
        );

        assert_eq!(
            history.redo_with_impact(&mut state),
            Some(EditImpact::TempoMap)
        );
        assert_eq!(state.tempo_map.points.len(), 1, "redo must put it back");
        assert!((state.tempo_map.points[0].bpm - 150.0).abs() < 1e-9);
    }

    /// Clearing automation folds the effective BPM into the fixed `bpm`, so an
    /// entry that only snapshotted the marker list would restore the markers but
    /// leave the project tempo at the folded value.
    #[test]
    fn clearing_tempo_automation_restores_the_fixed_bpm_too() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state
            .tempo_map
            .add_or_update_point(0.0, 90.0, TempoCurve::Hold);
        state
            .tempo_map
            .add_or_update_point(16.0, 140.0, TempoCurve::Hold);

        let prev = TempoStateSnapshot::capture(&state);
        state.bpm = 90.0;
        state
            .tempo_map
            .reset_to_single_point(0.0, 90.0, TempoCurve::Hold);
        let next = TempoStateSnapshot::capture(&state);
        let cmd = EditCommand::SetTempoState {
            label: "Clear Tempo Automation",
            prev,
            next,
        };

        cmd.undo(&mut state);
        assert_eq!(state.tempo_map.points.len(), 2);
        assert!(
            (state.bpm - 120.0).abs() < 1e-6,
            "fixed BPM must come back too"
        );

        cmd.execute(&mut state);
        assert_eq!(state.tempo_map.points.len(), 1);
        assert!((state.bpm - 90.0).abs() < 1e-6);
    }

    /// Restoring an older marker list must not rewind the revision counter that
    /// the renderer and engine caches diff against.
    #[test]
    fn tempo_undo_moves_the_map_revision_forward() {
        let mut state = TimelineState::default();
        let cmd = tempo_command(&mut state, "Add Tempo Marker");
        let after_edit = state.tempo_map.revision();
        cmd.undo(&mut state);
        assert!(
            state.tempo_map.revision() > after_edit,
            "revision must keep increasing so caches still invalidate"
        );
    }

    #[test]
    fn time_signature_marker_undoes_and_redoes() {
        let mut state = TimelineState::default();
        state.time_signature_map.ensure_default_point();
        let mut history = EditHistory::new(8);

        let prev = TimeSignatureStateSnapshot::capture(&state);
        state.add_time_signature_point(8.0, 7, 8);
        let next = TimeSignatureStateSnapshot::capture(&state);
        history.push(EditCommand::SetTimeSignatureState {
            label: "Add Time Signature",
            prev,
            next,
        });
        assert_eq!(state.time_signature_map.points.len(), 2);

        assert_eq!(
            history.undo_with_impact(&mut state),
            Some(EditImpact::TimeSignatureMap)
        );
        assert_eq!(state.time_signature_map.points.len(), 1);

        assert_eq!(
            history.redo_with_impact(&mut state),
            Some(EditImpact::TimeSignatureMap)
        );
        assert_eq!(state.time_signature_map.points.len(), 2);
        assert_eq!(state.time_signature_map.points[1].numerator, 7);
    }

    #[test]
    fn marker_and_region_edits_round_trip() {
        let mut state = TimelineState::default();
        let mut history = EditHistory::new(8);

        let prev_markers = state.markers.clone();
        state.add_marker_at_beat(12.0);
        history.push(EditCommand::SetMarkers {
            label: "Add Marker",
            prev: prev_markers,
            next: state.markers.clone(),
        });

        let prev_regions = state.regions.clone();
        let region_id = state.add_region_at_beat(4.0);
        history.push(EditCommand::SetRegions {
            label: "Add Region",
            prev: prev_regions,
            next: state.regions.clone(),
        });

        assert!(history.undo(&mut state));
        assert!(state.regions.is_empty(), "region undo comes first");
        assert_eq!(state.markers.len(), 1, "the marker entry is still below it");

        assert!(history.undo(&mut state));
        assert!(state.markers.is_empty());

        assert!(history.redo(&mut state));
        assert_eq!(state.markers.len(), 1);
        assert!(history.redo(&mut state));
        assert_eq!(state.regions.len(), 1);
        assert_eq!(state.regions[0].id, region_id);
    }

    #[test]
    fn region_drag_round_trips_through_one_entry() {
        let mut state = TimelineState::default();
        let id = state.add_region_at_beat(4.0);
        let prev = state.regions.clone();
        // Several drag frames, as the ruler produces them.
        for end in [9.0, 10.0, 11.5] {
            state.update_region_range(&id, 4.0, end);
        }
        let cmd = EditCommand::SetRegions {
            label: "Move Region",
            prev,
            next: state.regions.clone(),
        };
        assert!((state.regions[0].end_beat - 11.5).abs() < 1e-9);

        cmd.undo(&mut state);
        assert!(
            (state.regions[0].end_beat - 8.0).abs() < 1e-9,
            "one undo must return to the pre-drag range, not the previous frame"
        );
    }

    /// Lane heights are persisted view state: undoable like any other resize,
    /// and dirtying the project so the height survives a save — but never an
    /// engine-graph change.
    #[test]
    fn global_lane_height_is_undoable_persisted_view_state() {
        let mut state = TimelineState::default();
        let mut history = EditHistory::new(4);
        let prev = state.global_lane_heights.clone();
        let mut next = GlobalLaneHeights::default();
        next.set(GlobalLaneKind::Tempo, Some(120.0));

        history.push(EditCommand::SetGlobalLaneHeights {
            prev,
            next: next.clone(),
        });
        state.global_lane_heights = next;
        assert!((state.tempo_track_height() - 120.0).abs() < 0.01);

        assert_eq!(
            history.undo_with_impact(&mut state),
            Some(EditImpact::Metadata)
        );
        assert!(
            (state.tempo_track_height()
                - TimelineState::global_lane_default_height(GlobalLaneKind::Tempo))
            .abs()
                < 0.01
        );
    }

    /// Repeated taps in one tap-tempo session extend a single entry; an
    /// unrelated newest entry is never rewritten.
    #[test]
    fn tap_tempo_amends_its_own_entry_only() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let mut history = EditHistory::new(8);

        let prev = TempoStateSnapshot::capture(&state);
        state.bpm = 128.0;
        history.push(EditCommand::SetTempoState {
            label: "Tap Tempo",
            prev,
            next: TempoStateSnapshot::capture(&state),
        });

        state.bpm = 131.0;
        assert!(history.amend_tempo_state("Tap Tempo", TempoStateSnapshot::capture(&state)));
        assert!(
            history.undo(&mut state),
            "the whole session is still one step"
        );
        assert!((state.bpm - 120.0).abs() < 1e-6);
        assert!(history.redo(&mut state));
        assert!(
            (state.bpm - 131.0).abs() < 1e-6,
            "redo must land on the last tap, not the first"
        );

        // A marker edit on top must be untouchable by a later tap.
        history.push(EditCommand::SetMarkers {
            label: "Add Marker",
            prev: Vec::new(),
            next: vec![TimelineMarkerState::new(0.0, "A", "#ffffff")],
        });
        assert!(!history.amend_tempo_state("Tap Tempo", TempoStateSnapshot::capture(&state)));
    }

    #[test]
    fn region_snapshot_type_is_the_state_type() {
        // Guards the command against silently drifting from the state model.
        let region = TimelineRegionState::new(0.0, 4.0, "A", "#42C7A3");
        let cmd = EditCommand::SetRegions {
            label: "Add Region",
            prev: Vec::new(),
            next: vec![region.clone()],
        };
        let mut state = TimelineState::default();
        cmd.execute(&mut state);
        assert_eq!(state.regions, vec![region]);
        assert_eq!(cmd.impact(), EditImpact::Project);
    }
}

fn restore_track_snapshot(state: &mut TimelineState, snapshot: &TrackSnapshot) {
    if state
        .tracks
        .iter()
        .any(|track| track.id == snapshot.track.id)
    {
        return;
    }
    let index = snapshot.index.min(state.tracks.len());
    state.tracks.insert(index, snapshot.track.clone());
    state.selection.selected_track_id = Some(snapshot.track.id.clone());
    state.selection.selected_clip_ids.clear();
}

/// Bounded undo/redo stack.
#[derive(Debug, Clone, Default)]
pub struct EditHistory {
    undo_stack: VecDeque<EditCommand>,
    redo_stack: VecDeque<EditCommand>,
    max_steps: usize,
}

impl EditHistory {
    pub fn new(max_steps: usize) -> Self {
        Self {
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            max_steps: max_steps.max(1),
        }
    }

    pub fn push(&mut self, cmd: EditCommand) {
        self.undo_stack.push_back(cmd);
        if self.undo_stack.len() > self.max_steps {
            self.undo_stack.pop_front();
        }
        self.redo_stack.clear();
    }

    pub fn undo_with_impact(&mut self, state: &mut TimelineState) -> Option<EditImpact> {
        let cmd = self.undo_stack.pop_back()?;
        let impact = cmd.impact();
        cmd.undo(state);
        self.redo_stack.push_back(cmd);
        Some(impact)
    }

    pub fn undo(&mut self, state: &mut TimelineState) -> bool {
        self.undo_with_impact(state).is_some()
    }

    pub fn redo_with_impact(&mut self, state: &mut TimelineState) -> Option<EditImpact> {
        let cmd = self.redo_stack.pop_back()?;
        let impact = cmd.impact();
        cmd.execute(state);
        self.undo_stack.push_back(cmd);
        Some(impact)
    }

    pub fn redo(&mut self, state: &mut TimelineState) -> bool {
        self.redo_with_impact(state).is_some()
    }

    /// The command [`Self::undo_with_impact`] just reverted — it now sits on top
    /// of the redo stack. Borrowed rather than cloned: the caller only needs the
    /// `(track_id, param)` of a [`EditImpact::MixerControl`] entry, and several
    /// command variants carry whole clip/tempo snapshots.
    pub fn last_undone(&self) -> Option<&EditCommand> {
        self.redo_stack.back()
    }

    /// The command [`Self::redo_with_impact`] just re-applied — now on top of
    /// the undo stack. Counterpart to [`Self::last_undone`].
    pub fn last_redone(&self) -> Option<&EditCommand> {
        self.undo_stack.back()
    }

    /// Extend the newest entry when it is a tempo entry with `label`, instead
    /// of pushing another step. Lets a repeated gesture that fires many small
    /// commits — tap tempo taps in one session — stay a single undo step while
    /// still ending on the final value.
    ///
    /// Returns `false` when the newest entry is something else, so the caller
    /// falls back to a normal push and never rewrites an unrelated edit.
    pub fn amend_tempo_state(&mut self, label: &'static str, next: TempoStateSnapshot) -> bool {
        let Some(EditCommand::SetTempoState {
            label: top_label,
            next: top_next,
            ..
        }) = self.undo_stack.back_mut()
        else {
            return false;
        };
        if *top_label != label {
            return false;
        }
        *top_next = next;
        self.redo_stack.clear();
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }
}

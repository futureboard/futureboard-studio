//! The undo record for any edit to tracks: their settings, the track list
//! (tracks added, and their order), the master bus, and the project's audio
//! routing.
//!
//! One mechanism rather than a command per setting. An action names what it
//! is about to touch ([`TrackEditScope`]); the state is captured before and
//! after ([`TimelineState::capture_track_edit`] /
//! [`TimelineState::finish_track_edit`]), and a step puts one side back
//! ([`TimelineState::restore_track_edit`]). Tracks the action created are
//! picked up by comparing the track order, so "add a track" needs no scope of
//! its own.

use super::*;

/// What an action is about to change.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackEditScope {
    /// Tracks whose settings, inserts, sends, lanes or clips may change.
    pub tracks: Vec<String>,
    /// The master bus (volume, inserts).
    pub master: bool,
    /// The Audio Connections registry and the Master / Monitor Output
    /// assignments.
    pub routing: bool,
    /// The arrangement markers (a MIDI import can bring its own).
    pub markers: bool,
}

impl TrackEditScope {
    pub fn tracks<I, S>(ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            tracks: ids.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// One mixer channel: the master bus for [`MASTER_TRACK_ID`], the track
    /// otherwise.
    pub fn channel(id: &str) -> Self {
        if id == MASTER_TRACK_ID {
            Self::master()
        } else {
            Self::tracks([id])
        }
    }

    /// Just the track list: an action that only adds or reorders tracks.
    pub fn track_list() -> Self {
        Self::default()
    }

    pub fn master() -> Self {
        Self {
            master: true,
            ..Self::default()
        }
    }

    pub fn with_routing(mut self) -> Self {
        self.routing = true;
        self
    }

    pub fn with_markers(mut self) -> Self {
        self.markers = true;
        self
    }
}

/// An edit that has been opened (its "before" captured) and not yet recorded.
/// See `Timeline::begin_track_edit` / `Timeline::commit_track_edit`.
#[derive(Debug, Clone)]
pub struct PendingTrackEdit {
    pub scope: TrackEditScope,
    pub before: TrackEditSnapshot,
}

/// The project's audio routing, for a [`TrackEditSnapshot`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRoutingSnapshot {
    pub audio_connections: crate::audio_connections::AudioConnectionRegistry,
    pub master_output_connection_id: Option<crate::audio_connections::AudioConnectionId>,
    pub monitor_output_connection_id: Option<crate::audio_connections::AudioConnectionId>,
}

/// One side of a track edit.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackEditSnapshot {
    /// Every track id, in order.
    pub order: Vec<String>,
    /// The tracks in scope, whole.
    pub tracks: Vec<TrackState>,
    /// Tracks whose clips were left out because the edit did not change them:
    /// a restore keeps the clips they have. A settings click must not hold a
    /// copy of every note on the track.
    pub kept_clips: Vec<String>,
    /// Row heights of the tracks in scope; `None` is the default height.
    pub heights: Vec<(String, Option<f32>)>,
    pub master: Option<MasterBusState>,
    pub routing: Option<ProjectRoutingSnapshot>,
    pub markers: Option<Vec<TimelineMarkerState>>,
}

impl TrackEditSnapshot {
    /// Every loaded insert this side holds, as `(owner, insert)`.
    pub fn plugin_instances(&self) -> Vec<(String, String)> {
        let tracks = self.tracks.iter().flat_map(|track| {
            track
                .inserts
                .iter()
                .filter(|slot| !slot.is_empty())
                .map(|slot| (track.id.clone(), slot.id.clone()))
        });
        let master = self.master.iter().flat_map(|master| {
            master
                .inserts
                .iter()
                .filter(|slot| !slot.is_empty())
                .map(|slot| (MASTER_TRACK_ID.to_string(), slot.id.clone()))
        });
        tracks.chain(master).collect()
    }

    /// Every insert slot this side holds, for refreshing their stored state.
    pub fn slots_mut(&mut self) -> impl Iterator<Item = &mut InsertSlotState> {
        self.tracks
            .iter_mut()
            .flat_map(|track| track.inserts.iter_mut())
            .chain(self.master.iter_mut().flat_map(|m| m.inserts.iter_mut()))
    }
}

/// A track as far as an edit is concerned: without the level meters, which
/// move on their own while the action runs.
fn settled(track: &TrackState) -> TrackState {
    TrackState {
        meter_level_l: 0.0,
        meter_level_r: 0.0,
        meter_peak_hold_l: 0.0,
        meter_peak_hold_r: 0.0,
        meter_clip: false,
        ..track.clone()
    }
}

fn settled_master(master: &MasterBusState) -> MasterBusState {
    MasterBusState {
        meter_level_l: 0.0,
        meter_level_r: 0.0,
        meter_peak_hold_l: 0.0,
        meter_peak_hold_r: 0.0,
        meter_clip: false,
        // Derived from the routing for display.
        output_label: String::new(),
        ..master.clone()
    }
}

fn same_side(a: &TrackEditSnapshot, b: &TrackEditSnapshot) -> bool {
    a.order == b.order
        && a.heights == b.heights
        && a.routing == b.routing
        && a.markers == b.markers
        && a.master.as_ref().map(settled_master) == b.master.as_ref().map(settled_master)
        && a.tracks.len() == b.tracks.len()
        && a.tracks.iter().all(|track| {
            b.tracks
                .iter()
                .find(|other| other.id == track.id)
                .is_some_and(|other| settled(other) == settled(track))
        })
}

impl TimelineState {
    /// The state `scope` covers, as it stands.
    pub fn capture_track_edit(&self, scope: &TrackEditScope) -> TrackEditSnapshot {
        TrackEditSnapshot {
            order: self.tracks.iter().map(|track| track.id.clone()).collect(),
            tracks: scope
                .tracks
                .iter()
                .filter_map(|id| self.find_track(id).cloned())
                .collect(),
            kept_clips: Vec::new(),
            heights: scope
                .tracks
                .iter()
                .map(|id| (id.clone(), self.track_view_layout.height_for(id)))
                .collect(),
            master: scope.master.then(|| self.master.clone()),
            routing: scope.routing.then(|| ProjectRoutingSnapshot {
                audio_connections: self.audio_connections.clone(),
                master_output_connection_id: self.master_output_connection_id.clone(),
                monitor_output_connection_id: self.monitor_output_connection_id.clone(),
            }),
            markers: scope.markers.then(|| self.markers.clone()),
        }
    }

    /// Close an edit opened with [`Self::capture_track_edit`]: `(before,
    /// after)` for the undo record, or `None` when the action changed nothing.
    ///
    /// Tracks the action created are added to the scope, whole. Tracks on
    /// both sides whose clips did not change lose their clips from the
    /// record ([`TrackEditSnapshot::kept_clips`]).
    pub fn finish_track_edit(
        &self,
        scope: &TrackEditScope,
        mut before: TrackEditSnapshot,
    ) -> Option<(TrackEditSnapshot, TrackEditSnapshot)> {
        let mut after_scope = scope.clone();
        for track in &self.tracks {
            if !before.order.contains(&track.id) && !after_scope.tracks.contains(&track.id) {
                after_scope.tracks.push(track.id.clone());
            }
        }
        let mut after = self.capture_track_edit(&after_scope);
        if same_side(&before, &after) {
            return None;
        }
        let unchanged_clips: Vec<String> = before
            .tracks
            .iter()
            .filter(|track| {
                after
                    .tracks
                    .iter()
                    .any(|other| other.id == track.id && other.clips == track.clips)
            })
            .map(|track| track.id.clone())
            .collect();
        for side in [&mut before, &mut after] {
            for track in &mut side.tracks {
                if unchanged_clips.contains(&track.id) {
                    track.clips = Vec::new();
                }
            }
            side.kept_clips = unchanged_clips.clone();
        }
        Some((before, after))
    }

    /// Put one side of a track edit back: tracks it did not have go, the
    /// tracks it holds come back as they were (a track with
    /// [`TrackEditSnapshot::kept_clips`] keeps the clips it has; a live insert
    /// keeps its runtime fields), then the order, row heights, master bus and
    /// routing it recorded.
    pub fn restore_track_edit(&mut self, snapshot: &TrackEditSnapshot) {
        let gone: Vec<String> = self
            .tracks
            .iter()
            .filter(|track| !snapshot.order.contains(&track.id))
            .map(|track| track.id.clone())
            .collect();
        for id in gone {
            self.delete_track(&id);
        }
        for recorded in &snapshot.tracks {
            match self.tracks.iter_mut().find(|track| track.id == recorded.id) {
                Some(current) => {
                    let mut restored = recorded.clone();
                    if snapshot.kept_clips.contains(&recorded.id) {
                        restored.clips = std::mem::take(&mut current.clips);
                    }
                    restored.inserts =
                        super::track_clone::restored_slots(&recorded.inserts, &current.inserts);
                    // The ARA binding belongs to a live session that no step
                    // records; restoring an older one would unbind the track
                    // under its running plug-in.
                    restored.ara = current.ara.take();
                    restored.meter_level_l = current.meter_level_l;
                    restored.meter_level_r = current.meter_level_r;
                    restored.meter_peak_hold_l = current.meter_peak_hold_l;
                    restored.meter_peak_hold_r = current.meter_peak_hold_r;
                    restored.meter_clip = current.meter_clip;
                    *current = restored;
                }
                None => self.tracks.push(recorded.clone()),
            }
        }
        self.tracks.sort_by_key(|track| {
            snapshot
                .order
                .iter()
                .position(|id| *id == track.id)
                .unwrap_or(usize::MAX)
        });
        for (id, height) in &snapshot.heights {
            match height {
                Some(height) => self.track_view_layout.set_height(id.clone(), *height),
                None => self.track_view_layout.remove_track(id),
            }
        }
        if let Some(master) = &snapshot.master {
            let current = std::mem::replace(&mut self.master, master.clone());
            self.master.inserts =
                super::track_clone::restored_slots(&master.inserts, &current.inserts);
            self.master.meter_level_l = current.meter_level_l;
            self.master.meter_level_r = current.meter_level_r;
            self.master.meter_peak_hold_l = current.meter_peak_hold_l;
            self.master.meter_peak_hold_r = current.meter_peak_hold_r;
            self.master.meter_clip = current.meter_clip;
            self.master.output_label = current.output_label;
        }
        if let Some(routing) = &snapshot.routing {
            self.audio_connections = routing.audio_connections.clone();
            self.master_output_connection_id = routing.master_output_connection_id.clone();
            self.monitor_output_connection_id = routing.monitor_output_connection_id.clone();
            self.refresh_output_labels();
        }
        if let Some(markers) = &snapshot.markers {
            self.markers = markers.clone();
        }
        let known: std::collections::HashSet<String> =
            self.tracks.iter().map(|track| track.id.clone()).collect();
        if self
            .selection
            .selected_track_id
            .as_ref()
            .is_some_and(|id| !known.contains(id))
        {
            self.selection.selected_track_id = None;
        }
        self.selection
            .selected_track_ids
            .retain(|id| known.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::edit::edit_commands::{EditCommand, EditHistory};

    fn track(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    fn empty_state() -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state
    }

    fn order(state: &TimelineState) -> Vec<String> {
        state.tracks.iter().map(|t| t.id.clone()).collect()
    }

    /// Run `action` as an edit on `scope` and push the step it records.
    fn edit(
        state: &mut TimelineState,
        history: &mut EditHistory,
        scope: TrackEditScope,
        action: impl FnOnce(&mut TimelineState),
    ) -> bool {
        let before = state.capture_track_edit(&scope);
        action(state);
        let Some((prev, next)) = state.finish_track_edit(&scope, before) else {
            return false;
        };
        history.push(EditCommand::SetTracks {
            label: "Edit",
            prev: Box::new(prev),
            next: Box::new(next),
        });
        true
    }

    #[test]
    fn an_added_track_undoes_and_redoes() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let mut history = EditHistory::new(8);
        let mut added = String::new();
        assert!(edit(
            &mut state,
            &mut history,
            TrackEditScope::track_list(),
            |state| added = track(state, TrackType::Midi, "New"),
        ));
        assert_eq!(order(&state), vec![a.clone(), added.clone()]);

        assert!(history.undo(&mut state));
        assert_eq!(order(&state), vec![a.clone()]);
        assert!(history.redo(&mut state));
        assert_eq!(order(&state), vec![a, added.clone()]);
        assert_eq!(state.find_track(&added).unwrap().name, "New");
    }

    /// A settings click keeps no copy of the track's clips, and its undo
    /// leaves the clips the track has alone.
    #[test]
    fn a_toggle_undoes_without_carrying_the_clips() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Midi, "A");
        let clip = state.build_midi_clip(&a, 0.0, 4.0).expect("clip");
        state.tracks[0].clips.push(clip);
        let mut history = EditHistory::new(8);
        assert!(edit(
            &mut state,
            &mut history,
            TrackEditScope::tracks([a.clone()]),
            |state| {
                state.toggle_track_mute(&a);
            },
        ));
        match history.next_step(true).unwrap() {
            EditCommand::SetTracks { prev, next, .. } => {
                assert!(prev.tracks[0].clips.is_empty());
                assert!(next.tracks[0].clips.is_empty());
                assert_eq!(prev.kept_clips, vec![a.clone()]);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(state.tracks[0].muted);
        assert!(history.undo(&mut state));
        assert!(!state.tracks[0].muted);
        assert_eq!(state.tracks[0].clips.len(), 1, "the clips stay");
        assert!(history.redo(&mut state));
        assert!(state.tracks[0].muted);
        assert_eq!(state.tracks[0].clips.len(), 1);
    }

    /// An ARA binding made after a step is not the step's to undo: the
    /// session it names is still running.
    #[test]
    fn an_undo_keeps_the_ara_binding() {
        use crate::components::timeline::state::clip::AraTrackBinding;
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let mut history = EditHistory::new(8);
        assert!(edit(
            &mut state,
            &mut history,
            TrackEditScope::tracks([a.clone()]),
            |state| {
                state.toggle_track_mute(&a);
            },
        ));
        state.tracks[0].ara = Some(AraTrackBinding {
            plugin_id: "vst3:melodyne".to_string(),
            plugin_path: "/tmp/melodyne.vst3".to_string(),
            class_id: "class".to_string(),
        });
        assert!(history.undo(&mut state));
        assert!(!state.tracks[0].muted);
        assert!(state.tracks[0].ara.is_some(), "undo left ARA bound");
        assert!(history.redo(&mut state));
        assert!(state.tracks[0].ara.is_some(), "redo left ARA bound");
    }

    #[test]
    fn nothing_changed_records_nothing() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let mut history = EditHistory::new(8);
        assert!(!edit(
            &mut state,
            &mut history,
            TrackEditScope::tracks([a.clone()]),
            |state| {
                // Meters move on their own; that is not an edit.
                state.tracks[0].meter_level_l = 0.9;
            },
        ));
        assert!(!history.can_undo());
    }

    #[test]
    fn a_reorder_and_the_master_undo() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let b = track(&mut state, TrackType::Audio, "B");
        let mut history = EditHistory::new(8);
        assert!(edit(
            &mut state,
            &mut history,
            TrackEditScope::tracks([b.clone()]),
            |state| {
                state.reorder_track(&b, 0);
            },
        ));
        assert_eq!(order(&state), vec![b.clone(), a.clone()]);
        let before_volume = state.master.volume;
        assert!(edit(
            &mut state,
            &mut history,
            TrackEditScope::master(),
            |state| state.set_master_volume(0.25),
        ));
        assert!(history.undo(&mut state));
        assert_eq!(state.master.volume, before_volume);
        assert!(history.undo(&mut state));
        assert_eq!(order(&state), vec![a, b]);
    }

    /// Dragging through a colour picker is one step back to where it began.
    #[test]
    fn a_folded_gesture_is_one_step() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let original = state.tracks[0].color;
        let mut history = EditHistory::new(8);
        for shade in [0.2, 0.4, 0.6] {
            let scope = TrackEditScope::tracks([a.clone()]);
            let before = state.capture_track_edit(&scope);
            state.tracks[0].color.r = shade;
            let (prev, next) = state.finish_track_edit(&scope, before).unwrap();
            history.push_track_edit(
                EditCommand::SetTracks {
                    label: "Track Color",
                    prev: Box::new(prev),
                    next: Box::new(next),
                },
                true,
            );
        }
        assert!(history.undo(&mut state));
        assert_eq!(state.tracks[0].color, original);
        assert!(!history.can_undo());
        assert!(history.redo(&mut state));
        assert_eq!(state.tracks[0].color.r, 0.6);
    }

    /// Consecutive pans of one track fold; another track's is its own step.
    #[test]
    fn consecutive_pans_of_one_track_fold() {
        let mut state = empty_state();
        let a = track(&mut state, TrackType::Audio, "A");
        let b = track(&mut state, TrackType::Audio, "B");
        let mut history = EditHistory::new(8);
        let pan = |track: &str, prev: f32, next: f32| EditCommand::SetTrackPan {
            track_id: track.to_string(),
            prev,
            next,
        };
        for (prev, next) in [(0.0, 0.1), (0.1, 0.2), (0.2, 0.3)] {
            state.set_track_pan(&a, next);
            history.push_mixer_value(pan(&a, prev, next));
        }
        state.set_track_pan(&b, -0.5);
        history.push_mixer_value(pan(&b, 0.0, -0.5));
        assert!(history.undo(&mut state));
        assert!(history.undo(&mut state));
        assert_eq!(state.find_track(&a).unwrap().pan, 0.0);
        assert!(!history.can_undo());
    }
}

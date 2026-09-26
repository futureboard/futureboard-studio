//! Duplicating a track, and copying one channel's effect chain onto another.
//!
//! Both land new plug-in instances: every copied insert is
//! [`InsertSlotState::copy_as`] under a fresh id, carrying the state the caller
//! captured from the original. The model only builds and places the copies;
//! loading them is the caller's, as for any other added insert.

use std::collections::HashMap;
use std::sync::Arc;

use super::*;

/// What [`TimelineState::clone_track`] made.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackClone {
    /// The new track, right below the one it was cloned from.
    pub track_id: String,
    /// `(original insert id, copy's insert id)` for every slot the clone
    /// carries, in chain order.
    pub inserts: Vec<(String, String)>,
}

impl TrackState {
    /// Whether "Duplicate Track" can make a copy of this track.
    ///
    /// Not a Group (its members would have to come too), not the Master, not
    /// the Video track (a project holds one), and not a VSTi output channel —
    /// that belongs to its instrument and follows it.
    pub fn can_be_cloned(&self) -> bool {
        !matches!(
            self.track_type,
            TrackType::Group | TrackType::Master | TrackType::Video
        ) && !is_vsti_output_child_track_id(&self.id)
    }

    /// The slots a clone of this track copies: all of them `with_fx`, and
    /// otherwise only those below the chain's floor — the instrument, so a
    /// clone of an instrument track still plays.
    pub fn clone_insert_range(&self, with_fx: bool) -> std::ops::Range<usize> {
        let end = if with_fx {
            self.inserts.len()
        } else {
            self.fx_chain_floor().min(self.inserts.len())
        };
        0..end
    }
}

impl TimelineState {
    /// Copy `source_id` into a new track right below it, and return what was
    /// made. `None` when the track does not exist or cannot be cloned (see
    /// [`TrackState::can_be_cloned`]).
    ///
    /// The copy has the source's settings, routing, sends and clips (fresh
    /// ids throughout, take history following its clips), and the plug-ins in
    /// [`TrackState::clone_insert_range`], each with the state in `states`
    /// keyed by the original's insert id (its stored state when absent).
    /// Automation comes too, retargeted at the copies; a lane on a plug-in
    /// that was left behind is dropped.
    ///
    /// Not carried: the ARA binding (its edits are an archive per plug-in and
    /// track, which a new track does not have), record arm, solo, listen and
    /// input monitoring — two tracks armed on one input record it twice.
    pub fn clone_track(
        &mut self,
        source_id: &str,
        with_fx: bool,
        states: &HashMap<String, Arc<Vec<u8>>>,
    ) -> Option<TrackClone> {
        let index = self.tracks.iter().position(|track| track.id == source_id)?;
        if !self.tracks[index].can_be_cloned() {
            return None;
        }
        let source = self.tracks[index].clone();
        let track_id = self.next_track_id();

        let mut insert_map: Vec<(String, String)> = Vec::new();
        let mut inserts = Vec::new();
        for slot in &source.inserts[source.clone_insert_range(with_fx)] {
            let copy_id = self.next_insert_slot_id_for(&track_id);
            insert_map.push((slot.id.clone(), copy_id.clone()));
            inserts.push(slot.copy_as(copy_id, states.get(&slot.id).cloned()));
        }
        let new_insert_id = |old: &str| {
            insert_map
                .iter()
                .find(|(original, _)| original == old)
                .map(|(_, copy)| copy.clone())
        };

        let send_map: Vec<(String, String)> = source
            .sends
            .iter()
            .enumerate()
            .map(|(position, send)| (send.id.clone(), format!("send-{track_id}-{}", position + 1)))
            .collect();
        let sends = source
            .sends
            .iter()
            .zip(&send_map)
            .map(|(send, (_, id))| SendSlotState {
                id: id.clone(),
                ..send.clone()
            })
            .collect();
        let retarget = |target: &AutomationTarget| -> Option<AutomationTarget> {
            Some(match target {
                AutomationTarget::PluginParameter {
                    insert_id,
                    parameter_id,
                    parameter_name,
                } => AutomationTarget::PluginParameter {
                    insert_id: new_insert_id(insert_id)?,
                    parameter_id: parameter_id.clone(),
                    parameter_name: parameter_name.clone(),
                },
                AutomationTarget::SendLevel { send_id } => AutomationTarget::SendLevel {
                    send_id: send_map
                        .iter()
                        .find(|(original, _)| original == send_id)
                        .map(|(_, copy)| copy.clone())?,
                },
                other => other.clone(),
            })
        };
        let automation_lanes = source
            .automation_lanes
            .iter()
            .filter_map(|lane| {
                Some(AutomationLaneState {
                    target: retarget(&lane.target)?,
                    ..lane.clone()
                })
            })
            .collect();
        let selected_automation_target = source
            .selected_automation_target
            .as_ref()
            .and_then(retarget);

        let mut next_clip = next_clip_id_number(&self.tracks);
        let mut clip_map: HashMap<String, String> = HashMap::new();
        let clips = source
            .clips
            .iter()
            .map(|clip| {
                let id = format!("clip-{next_clip}");
                next_clip += 1;
                clip_map.insert(clip.id.clone(), id.clone());
                ClipState { id, ..clip.clone() }
            })
            .collect();
        let takes: Vec<TrackTake> = source
            .takes
            .iter()
            .filter_map(|take| {
                Some(TrackTake {
                    clip_id: clip_map.get(&take.clip_id)?.clone(),
                    ..take.clone()
                })
            })
            .collect();

        let instrument_plugin_instance_id = source
            .instrument_plugin_instance_id
            .as_deref()
            .and_then(new_insert_id);
        let takes_expanded = source.takes_expanded && !takes.is_empty();
        let copied_insert_ids: Vec<String> = insert_map
            .iter()
            .map(|(original, _)| original.clone())
            .collect();
        let clone = TrackState {
            id: track_id.clone(),
            name: format!("{} Copy", source.name),
            ara: None,
            solo: false,
            armed: false,
            listen: ListenMode::Off,
            input_monitor: InputMonitorMode::Off,
            meter_level_l: 0.0,
            meter_level_r: 0.0,
            meter_peak_hold_l: 0.0,
            meter_peak_hold_r: 0.0,
            meter_clip: false,
            clips,
            takes,
            takes_expanded,
            automation_lanes,
            selected_automation_target,
            inserts,
            instrument_plugin_instance_id,
            sends,
            ..source
        };

        // Below the source and below any VSTi output channels that belong to
        // it, so the clone never splits an instrument from its outputs.
        let mut at = index + 1;
        while self.tracks.get(at).is_some_and(|track| {
            vsti_output_child_insert_id(&track.id)
                .is_some_and(|insert| copied_insert_ids.iter().any(|id| id == insert))
        }) {
            at += 1;
        }
        self.tracks.insert(at, clone);
        if let Some(height) = self.track_view_layout.height_for(source_id) {
            self.track_view_layout.set_height(track_id.clone(), height);
        }
        Some(TrackClone {
            track_id,
            inserts: insert_map,
        })
    }

    /// The ids of `track_id`'s effect slots: every slot at or above the
    /// chain's floor. Empty for an unknown track.
    pub fn fx_chain_ids(&self, track_id: &str) -> Vec<String> {
        let floor = self.fx_chain_floor(track_id);
        self.insert_slots(track_id)
            .map(|slots| {
                slots
                    .iter()
                    .skip(floor)
                    .map(|slot| slot.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The effects `track_id`'s chain would hand to "Copy FX Chain", in chain
    /// order: the loaded slots above the floor. Never an instrument.
    pub fn fx_chain_effects(&self, track_id: &str) -> Vec<InsertSlotState> {
        let floor = self.fx_chain_floor(track_id);
        self.insert_slots(track_id)
            .map(|slots| {
                slots
                    .iter()
                    .skip(floor)
                    .filter(|slot| !slot.is_empty() && slot.plugin_is_instrument != Some(true))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `track_id`'s effects can be replaced by a chain of `count`.
    ///
    /// The chain has to fit beside the instrument slot, and the channel has
    /// to carry audio: a Video or Group track has no chain, and a MIDI track
    /// makes no audio until an instrument does.
    pub fn can_take_fx_chain(&self, track_id: &str, count: usize) -> bool {
        if count == 0 {
            return false;
        }
        if track_id == MASTER_TRACK_ID {
            return count <= MAX_INSERT_SLOTS;
        }
        let Some(track) = self.find_track(track_id) else {
            return false;
        };
        match track.track_type {
            TrackType::Video | TrackType::Group | TrackType::Master => false,
            TrackType::Midi if track.fx_chain_floor() == 0 => false,
            _ => track.fx_chain_floor() + count <= MAX_INSERT_SLOTS,
        }
    }

    /// Replace `track_id`'s effects with copies of `effects`, keeping its
    /// instrument, and return the copies' ids in chain order. `None`, changing
    /// nothing, when the chain cannot take them (see
    /// [`Self::can_take_fx_chain`]).
    ///
    /// Each copy is [`InsertSlotState::copy_as`] with its template's own
    /// stored state. The removed effects go the way [`Self::remove_insert`]
    /// takes any insert, their automation lanes with them; the caller tears
    /// their instances down first.
    pub fn replace_fx_chain(
        &mut self,
        track_id: &str,
        effects: &[InsertSlotState],
    ) -> Option<Vec<String>> {
        if !self.can_take_fx_chain(track_id, effects.len()) {
            return None;
        }
        for insert_id in self.fx_chain_ids(track_id) {
            self.remove_insert(track_id, &insert_id);
        }
        let placeholder = track_id != MASTER_TRACK_ID
            && self
                .find_track(track_id)
                .is_some_and(TrackState::needs_instrument_placeholder);
        if placeholder {
            let placeholder_id = self.next_insert_slot_id_for(track_id);
            self.insert_slots_mut(track_id)?
                .push(InsertSlotState::empty(placeholder_id));
        }
        let mut copies = Vec::with_capacity(effects.len());
        for effect in effects {
            let copy_id = self.next_insert_slot_id_for(track_id);
            let copy = effect.copy_as(copy_id.clone(), None);
            self.insert_slots_mut(track_id)?.push(copy);
            copies.push(copy_id);
        }
        Some(copies)
    }

    /// `track_id`'s insert chain as it stands, for an undo step that puts it
    /// back. `None` for an unknown channel.
    pub fn capture_insert_chain(&self, track_id: &str) -> Option<InsertChainSnapshot> {
        let inserts = self.insert_slots(track_id)?.clone();
        let track = self.find_track(track_id);
        let output_tracks = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| {
                vsti_output_child_insert_id(&track.id)
                    .is_some_and(|owner| inserts.iter().any(|slot| slot.id == owner))
            })
            .map(|(index, track)| (index, track.clone()))
            .collect();
        Some(InsertChainSnapshot {
            track_id: track_id.to_string(),
            automation_lanes: track
                .map(|track| track.automation_lanes.clone())
                .unwrap_or_default(),
            selected_automation_target: track
                .and_then(|track| track.selected_automation_target.clone()),
            instrument_plugin_instance_id: track
                .and_then(|track| track.instrument_plugin_instance_id.clone()),
            inserts,
            output_tracks,
        })
    }

    /// Put `snapshot`'s chain back on its channel: the slots, the track's
    /// automation and instrument pointer, and the VSTi output channels of its
    /// inserts where they stood. Output channels of the chain being replaced
    /// go first.
    ///
    /// A slot whose instance is still live — the same id before and after —
    /// keeps its current runtime fields (load status, host, parameters,
    /// detected outputs): the instance did not change, only the chain around
    /// it did, and a snapshot's copy of them is only as new as the snapshot.
    pub fn restore_insert_chain(&mut self, snapshot: &InsertChainSnapshot) {
        let Some(current) = self.insert_slots(&snapshot.track_id).cloned() else {
            return;
        };
        let owned_by_either = |insert: &str| {
            current.iter().any(|slot| slot.id == insert)
                || snapshot.inserts.iter().any(|slot| slot.id == insert)
        };
        self.tracks
            .retain(|track| !vsti_output_child_insert_id(&track.id).is_some_and(owned_by_either));
        let inserts = restored_slots(&snapshot.inserts, &current);
        if let Some(slots) = self.insert_slots_mut(&snapshot.track_id) {
            *slots = inserts;
        }
        if let Some(track) = self
            .tracks
            .iter_mut()
            .find(|track| track.id == snapshot.track_id)
        {
            track.automation_lanes = snapshot.automation_lanes.clone();
            track.selected_automation_target = snapshot.selected_automation_target.clone();
            track.instrument_plugin_instance_id = snapshot.instrument_plugin_instance_id.clone();
        }
        for (index, track) in &snapshot.output_tracks {
            let at = (*index).min(self.tracks.len());
            self.tracks.insert(at, track.clone());
        }
    }
}

/// `snapshot`'s slots as a restore puts them back over `current`: a slot
/// whose instance is still live (same id) keeps its current runtime fields —
/// load status, host, parameters, detected outputs — because the instance did
/// not change and the snapshot's copy is only as new as the snapshot.
pub(super) fn restored_slots(
    snapshot: &[InsertSlotState],
    current: &[InsertSlotState],
) -> Vec<InsertSlotState> {
    snapshot
        .iter()
        .map(
            |slot| match current.iter().find(|live| live.id == slot.id) {
                Some(live) => InsertSlotState {
                    load_status: live.load_status.clone(),
                    runtime_backend: live.runtime_backend.clone(),
                    runtime_state: live.runtime_state.clone(),
                    host_pid: live.host_pid,
                    parameters: live.parameters.clone(),
                    output_bus_channel_counts: live.output_bus_channel_counts.clone(),
                    ..slot.clone()
                },
                None => slot.clone(),
            },
        )
        .collect()
}

/// One channel's insert chain as it stood, for [`TimelineState::restore_insert_chain`].
///
/// Whole-chain rather than one slot: adding, removing or replacing a plug-in
/// also moves its neighbours, can add an instrument placeholder, and drops the
/// removed plug-in's automation lanes and output channels — a per-slot record
/// would have to re-derive all of that to undo it.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertChainSnapshot {
    pub track_id: String,
    /// Every slot, each with its plug-in state as it was when captured.
    pub inserts: Vec<InsertSlotState>,
    /// The track's lanes (none for the master), plug-in parameter lanes
    /// included.
    pub automation_lanes: Vec<AutomationLaneState>,
    pub selected_automation_target: Option<AutomationTarget>,
    pub instrument_plugin_instance_id: Option<String>,
    /// VSTi output channels of these inserts, with their place in the track
    /// list.
    pub output_tracks: Vec<(usize, TrackState)>,
}

impl InsertChainSnapshot {
    /// The loaded slots' insert ids.
    pub fn plugin_ids(&self) -> impl Iterator<Item = &str> {
        self.inserts
            .iter()
            .filter(|slot| !slot.is_empty())
            .map(|slot| slot.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn load(
        state: &mut TimelineState,
        track_id: &str,
        index: usize,
        name: &str,
        instrument: bool,
    ) -> String {
        let slot = state.ensure_insert_slot_at(track_id, index).expect("slot");
        state.set_insert_plugin(
            track_id,
            &slot,
            name.to_string(),
            Some(std::path::PathBuf::from(format!("C:/p/{name}.vst3"))),
            InsertPluginFormat::Vst3,
            None,
            name.to_string(),
        );
        state.set_insert_plugin_role(track_id, &slot, instrument);
        slot
    }

    fn param_lane(id: &str, insert_id: &str) -> AutomationLaneState {
        AutomationLaneState::new(
            id,
            AutomationTarget::PluginParameter {
                insert_id: insert_id.to_string(),
                parameter_id: "7".to_string(),
                parameter_name: "Cutoff".to_string(),
            },
        )
    }

    fn midi_clip(id: &str) -> ClipState {
        ClipState {
            id: id.to_string(),
            name: id.to_string(),
            start_beat: 0.0,
            duration_beats: 4.0,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Midi {
                notes: Vec::new(),
                controller_lanes: Vec::new(),
                sysex_events: Vec::new(),
                articulations: Vec::new(),
            },
            muted: false,
            audio_import: AudioImportState::default(),
            stretch: AudioClipStretchState::default(),
        }
    }

    fn empty_state() -> TimelineState {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state
    }

    fn track_mut<'a>(state: &'a mut TimelineState, track_id: &str) -> &'a mut TrackState {
        state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track")
    }

    /// A MIDI track with a VSTi, an effect, a clip with a take, and a lane on
    /// each plug-in.
    fn instrument_track(state: &mut TimelineState) -> (String, String, String) {
        let midi = track(state, TrackType::Midi, "Keys");
        let synth = load(state, &midi, 0, "synth", true);
        let fx = load(state, &midi, 1, "fx", false);
        let track = track_mut(state, &midi);
        track.clips.push(midi_clip("clip-1"));
        track
            .takes
            .push(TrackTake::new("take-1", "Take 1", "clip-1"));
        track.automation_lanes.push(param_lane("synth-cut", &synth));
        track.automation_lanes.push(param_lane("fx-cut", &fx));
        track.automation_lanes.push(AutomationLaneState::new(
            "vol",
            AutomationTarget::TrackVolume,
        ));
        track.armed = true;
        (midi, synth, fx)
    }

    #[test]
    fn a_clone_with_fx_copies_every_plugin_as_a_new_instance() {
        let mut state = empty_state();
        let (midi, synth, fx) = instrument_track(&mut state);
        let after = track(&mut state, TrackType::Audio, "After");
        let captured = Arc::new(vec![7u8, 7]);
        let states = HashMap::from([(fx.clone(), captured.clone())]);

        let clone = state.clone_track(&midi, true, &states).expect("cloned");

        let order: Vec<_> = state.tracks.iter().map(|t| t.id.clone()).collect();
        assert_eq!(order, vec![midi.clone(), clone.track_id.clone(), after]);
        let copy = state.find_track(&clone.track_id).unwrap();
        assert_eq!(copy.name, "Keys Copy");
        assert!(!copy.armed, "a clone is never armed on its source's input");
        assert_eq!(clone.inserts.len(), 2);
        let (synth_copy, fx_copy) = (&clone.inserts[0].1, &clone.inserts[1].1);
        assert_ne!(synth_copy, &synth);
        assert_ne!(fx_copy, &fx);
        assert_eq!(
            state.insert_owner_ids_containing(fx_copy),
            vec![clone.track_id.clone()]
        );
        assert_eq!(
            copy.instrument_plugin_instance_id.as_deref(),
            Some(synth_copy.as_str())
        );
        assert!(Arc::ptr_eq(
            copy.inserts[1].vst3_state.as_ref().unwrap(),
            &captured
        ));

        // Clips get fresh ids, and the take follows its clip.
        assert_eq!(copy.clips.len(), 1);
        assert_ne!(copy.clips[0].id, "clip-1");
        assert_eq!(copy.takes[0].clip_id, copy.clips[0].id);

        // Lanes are retargeted at the copies.
        let targets: Vec<_> = copy
            .automation_lanes
            .iter()
            .map(|lane| match &lane.target {
                AutomationTarget::PluginParameter { insert_id, .. } => insert_id.clone(),
                other => other.display_name(),
            })
            .collect();
        assert_eq!(
            targets,
            vec![synth_copy.clone(), fx_copy.clone(), "Volume".into()]
        );

        // The source is untouched.
        assert_eq!(state.insert_order(&midi), vec![synth, fx]);
        assert!(state.find_track(&midi).unwrap().armed);
    }

    #[test]
    fn a_clone_without_fx_keeps_only_the_instrument() {
        let mut state = empty_state();
        let (midi, synth, _fx) = instrument_track(&mut state);

        let clone = state
            .clone_track(&midi, false, &HashMap::new())
            .expect("cloned");

        assert_eq!(clone.inserts.len(), 1);
        assert_eq!(clone.inserts[0].0, synth);
        let copy = state.find_track(&clone.track_id).unwrap();
        assert_eq!(copy.inserts.len(), 1);
        assert_eq!(copy.inserts[0].plugin_is_instrument, Some(true));
        let lanes: Vec<_> = copy.automation_lanes.iter().map(|l| l.id.clone()).collect();
        assert_eq!(
            lanes,
            vec!["synth-cut", "vol"],
            "the effect's lane stays behind"
        );

        // An audio track has no floor: nothing comes.
        let audio = track(&mut state, TrackType::Audio, "Audio");
        load(&mut state, &audio, 0, "fx", false);
        let bare = state
            .clone_track(&audio, false, &HashMap::new())
            .expect("cloned");
        assert!(state.find_track(&bare.track_id).unwrap().inserts.is_empty());
    }

    #[test]
    fn tracks_a_clone_cannot_stand_for_are_refused() {
        let mut state = empty_state();
        let group = track(&mut state, TrackType::Group, "Group");
        assert_eq!(state.clone_track(&group, true, &HashMap::new()), None);
        assert_eq!(state.clone_track("missing", true, &HashMap::new()), None);
    }

    #[test]
    fn pasting_a_chain_replaces_the_effects_and_keeps_the_instrument() {
        let mut state = empty_state();
        let audio = track(&mut state, TrackType::Audio, "Audio");
        let eq = load(&mut state, &audio, 0, "eq", false);
        let comp = load(&mut state, &audio, 1, "comp", false);
        let chain = state.fx_chain_effects(&audio);
        assert_eq!(chain.len(), 2);

        let (midi, synth, old_fx) = instrument_track(&mut state);
        let copies = state.replace_fx_chain(&midi, &chain).expect("pasted");

        let order = state.insert_order(&midi);
        assert_eq!(order.len(), 3);
        assert_eq!(order[0], synth, "the instrument stays first");
        assert_eq!(order[1..], copies[..]);
        assert!(!order.contains(&old_fx));
        assert!(!copies.contains(&eq) && !copies.contains(&comp));
        let names: Vec<_> = state.insert_slots(&midi).unwrap()[1..]
            .iter()
            .map(|slot| slot.display_name.clone())
            .collect();
        assert_eq!(names, vec!["eq", "comp"]);
        let lanes: Vec<_> = state
            .find_track(&midi)
            .unwrap()
            .automation_lanes
            .iter()
            .map(|l| l.id.clone())
            .collect();
        assert_eq!(
            lanes,
            vec!["synth-cut", "vol"],
            "the removed effect's lane went with it"
        );
        // The source chain is untouched.
        assert_eq!(state.insert_order(&audio), vec![eq, comp]);
    }

    #[test]
    fn a_chain_goes_only_where_it_fits_and_makes_sound() {
        let mut state = empty_state();
        let audio = track(&mut state, TrackType::Audio, "Audio");
        load(&mut state, &audio, 0, "fx", false);
        let chain = state.fx_chain_effects(&audio);

        let bare_midi = track(&mut state, TrackType::Midi, "Bare");
        assert!(!state.can_take_fx_chain(&bare_midi, 1));
        assert_eq!(state.replace_fx_chain(&bare_midi, &chain), None);

        let inst = track(&mut state, TrackType::Instrument, "Inst");
        assert!(state.can_take_fx_chain(&inst, MAX_INSERT_SLOTS - 1));
        assert!(!state.can_take_fx_chain(&inst, MAX_INSERT_SLOTS));
        let copies = state.replace_fx_chain(&inst, &chain).expect("pasted");
        assert!(
            state.insert_slot_at(&inst, 0).unwrap().is_empty(),
            "slot 0 is kept for the instrument"
        );
        assert_eq!(state.insert_order(&inst)[1..], copies[..]);

        assert!(
            !state.can_take_fx_chain(&audio, 0),
            "an empty chain pastes nothing"
        );
    }

    use crate::components::edit::edit_commands::{EditCommand, EditHistory, TrackSnapshot};

    fn ids(pairs: Vec<(String, String)>) -> Vec<String> {
        pairs.into_iter().map(|(_, id)| id).collect()
    }

    /// A pasted chain is one step: undo puts the replaced effects back —
    /// with their lanes and the state they had — and says which instances
    /// go and which come back; redo does the opposite.
    #[test]
    fn a_pasted_chain_undoes_and_redoes_as_one_step() {
        let mut state = empty_state();
        let audio = track(&mut state, TrackType::Audio, "Audio");
        load(&mut state, &audio, 0, "eq", false);
        let chain = state.fx_chain_effects(&audio);
        let (midi, synth, old_fx) = instrument_track(&mut state);
        let before = vec![state.capture_insert_chain(&midi).unwrap()];
        let copies = state.replace_fx_chain(&midi, &chain).expect("pasted");
        let after = vec![state.capture_insert_chain(&midi).unwrap()];
        let pasted = state.insert_order(&midi);

        let mut history = EditHistory::new(8);
        history.push(EditCommand::SetInsertChains {
            label: "Paste FX Chain",
            prev: before,
            next: after,
        });
        let step = history.next_step(true).unwrap();
        assert_eq!(ids(step.instances_leaving(true)), copies);
        assert_eq!(ids(step.instances_arriving(true)), vec![old_fx.clone()]);

        // The replaced effect's live state, captured right before the step.
        let live = Arc::new(vec![4u8, 2]);
        history
            .next_step_mut(true)
            .unwrap()
            .refresh_plugin_states(&HashMap::from([(old_fx.clone(), live.clone())]));

        assert!(history.undo(&mut state));
        assert_eq!(
            state.insert_order(&midi),
            vec![synth.clone(), old_fx.clone()]
        );
        let lanes: Vec<_> = state
            .find_track(&midi)
            .unwrap()
            .automation_lanes
            .iter()
            .map(|l| l.id.clone())
            .collect();
        assert_eq!(
            lanes,
            vec!["synth-cut", "fx-cut", "vol"],
            "its lane is back"
        );
        assert!(Arc::ptr_eq(
            state
                .find_insert_slot(&midi, &old_fx)
                .unwrap()
                .vst3_state
                .as_ref()
                .unwrap(),
            &live
        ));

        let step = history.next_step(false).unwrap();
        assert_eq!(ids(step.instances_leaving(false)), vec![old_fx]);
        assert_eq!(ids(step.instances_arriving(false)), copies);
        assert!(history.redo(&mut state));
        assert_eq!(state.insert_order(&midi), pasted);
    }

    /// A slot that stays live across a step keeps its runtime fields: only
    /// the chain around it changed, not the instance.
    #[test]
    fn restoring_a_chain_keeps_a_live_instance_as_it_is() {
        let mut state = empty_state();
        let (midi, synth, fx) = instrument_track(&mut state);
        let before = state.capture_insert_chain(&midi).unwrap();
        state.remove_insert(&midi, &fx);
        {
            let slot = state
                .insert_slots_mut(&midi)
                .unwrap()
                .iter_mut()
                .find(|slot| slot.id == synth)
                .unwrap();
            slot.runtime_state = PluginRuntimeState::EditorOpen;
            slot.host_pid = Some(7);
        }
        state.restore_insert_chain(&before);
        assert_eq!(state.insert_order(&midi), vec![synth.clone(), fx]);
        let live = state.find_insert_slot(&midi, &synth).unwrap();
        assert_eq!(live.runtime_state, PluginRuntimeState::EditorOpen);
        assert_eq!(live.host_pid, Some(7));
    }

    #[test]
    fn a_duplicated_track_undoes_and_redoes() {
        let mut state = empty_state();
        let (midi, _synth, _fx) = instrument_track(&mut state);
        let clone = state
            .clone_track(&midi, true, &HashMap::new())
            .expect("cloned");
        let snapshot = TrackSnapshot::capture(&state, &clone.track_id).unwrap();
        let mut history = EditHistory::new(8);
        history.push(EditCommand::DuplicateTrack {
            snapshot,
            height: Some(120.0),
        });

        let copies: Vec<String> = clone.inserts.iter().map(|(_, id)| id.clone()).collect();
        assert_eq!(
            ids(history.next_step(true).unwrap().instances_leaving(true)),
            copies
        );
        assert!(history.undo(&mut state));
        assert!(state.find_track(&clone.track_id).is_none());

        assert_eq!(
            ids(history.next_step(false).unwrap().instances_arriving(false)),
            copies
        );
        assert!(history.redo(&mut state));
        let order: Vec<_> = state.tracks.iter().map(|t| t.id.clone()).collect();
        assert_eq!(order, vec![midi, clone.track_id.clone()]);
        assert_eq!(
            state.track_view_layout.height_for(&clone.track_id),
            Some(120.0)
        );
    }

    /// Undoing a delete brings the track's plug-ins back, so the studio has
    /// to load them — it used to leave them silent.
    #[test]
    fn undoing_a_track_delete_brings_its_plugins_back() {
        let mut state = empty_state();
        let (midi, synth, fx) = instrument_track(&mut state);
        let snapshot = TrackSnapshot::capture(&state, &midi).unwrap();
        let command = EditCommand::DeleteTrack { snapshot };
        assert_eq!(
            ids(command.instances_arriving(true)),
            vec![synth.clone(), fx.clone()]
        );
        assert_eq!(ids(command.instances_leaving(false)), vec![synth, fx]);
        assert!(command.instances_leaving(true).is_empty());
    }

    /// Stepping through presets on one insert folds into one step back to
    /// where it started; a change to another insert is its own step.
    #[test]
    fn consecutive_preset_loads_on_one_insert_are_one_step() {
        let mut state = empty_state();
        let audio = track(&mut state, TrackType::Audio, "Audio");
        let fx = load(&mut state, &audio, 0, "fx", false);
        let other = load(&mut state, &audio, 1, "other", false);
        let original = Some(Arc::new(vec![0u8]));
        let preset = |n: u8| Some(Arc::new(vec![n]));
        let load_preset = |insert: &str, prev, next| EditCommand::SetInsertState {
            label: "Load Preset",
            track_id: audio.clone(),
            insert_id: insert.to_string(),
            prev,
            next,
        };

        let mut history = EditHistory::new(8);
        history.push_insert_state(load_preset(&fx, original.clone(), preset(1)));
        history.push_insert_state(load_preset(&fx, preset(1), preset(2)));
        history.push_insert_state(load_preset(&fx, preset(2), preset(3)));
        history.push_insert_state(load_preset(&other, None, preset(9)));

        assert!(history.undo(&mut state), "the other insert's load");
        assert!(history.undo(&mut state), "all three loads on fx");
        assert_eq!(
            state.find_insert_slot(&audio, &fx).unwrap().vst3_state,
            original
        );
        assert!(!history.can_undo());
        assert!(history.redo(&mut state));
        assert_eq!(
            state.find_insert_slot(&audio, &fx).unwrap().vst3_state,
            preset(3)
        );
    }

    #[test]
    fn lowering_the_step_limit_drops_the_oldest_steps_at_once() {
        let mut state = empty_state();
        let audio = track(&mut state, TrackType::Audio, "Audio");
        let fx = load(&mut state, &audio, 0, "fx", false);
        let mut history = EditHistory::new(10);
        for n in 0..6u8 {
            history.push(EditCommand::SetInsertState {
                label: "Paste Plug-in State",
                track_id: audio.clone(),
                insert_id: fx.clone(),
                prev: Some(Arc::new(vec![n])),
                next: Some(Arc::new(vec![n + 1])),
            });
        }
        history.set_max_steps(2);
        assert_eq!(history.undo_label(), Some("Paste Plug-in State"));
        assert!(history.undo(&mut state));
        assert!(history.undo(&mut state));
        assert!(!history.undo(&mut state), "only the newest two were kept");
        assert_eq!(history.redo_label(), Some("Paste Plug-in State"));
    }
}

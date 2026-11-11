//! Automation recording: with the Automation Arm on and the transport
//! playing, moving a control writes its lane.
//!
//! Driven from the playback tick (UI thread, display rate), not the audio
//! thread. Each tick reads the control's own value — the base value the user
//! moves, never the automated one the fader displays — and, while a lane is
//! writing, replaces its points from where writing reached to the playhead.
//!
//! Writing is touch-style: it starts when the control is grabbed and stops
//! when it is let go, and the lane plays again from there. Studio's controls
//! (faders, pans, sends, switches, Studio's parameter editor) do not all
//! report a held gesture, so a control counts as touched while it moves and is
//! released after [`TOUCH_RELEASE_SECONDS`] of standing still. A plug-in's own
//! window does not report its edits to Studio yet, so it records nothing. An
//! untouched lane
//! is never written, and a disarmed transport writes nothing at all.
//!
//! While a lane writes it is marked `recording`, and the engine snapshot
//! leaves it out: what is heard is the moving control, and what is written is
//! what was heard. Writing a lane that was overridden by hand puts it back in
//! play. The whole run is one undo step per track.

use std::collections::HashMap;

use super::*;

/// How long a control must stand still in Touch before the lane takes over
/// again.
pub const TOUCH_RELEASE_SECONDS: f64 = 0.5;

/// Values closer than this are the same value: a flat stretch is extended, not
/// re-pointed every tick.
const VALUE_EPS: f32 = 1.0e-4;

/// Beat gap between the lane's value and the recorded one at a punch-in or a
/// release, so the change is a step rather than two points on one beat.
const PUNCH_GAP_BEATS: f32 = 1.0e-3;

#[derive(Debug, Clone)]
struct LanePass {
    track_id: String,
    lane_id: String,
    /// The control's value last tick.
    last_value: f32,
    /// When the control last changed, in seconds on the caller's clock.
    last_change: f64,
    /// Whether this lane is being written right now.
    writing: bool,
    /// How far writing has reached; the next tick replaces from here.
    written_to: f32,
    /// The last point this pass wrote, and whether it extends a flat stretch
    /// (so the next equal value moves it instead of adding another).
    last_point: Option<(u64, f32, bool)>,
    /// The lane as it was before the run, for what a Touch release returns to.
    original: Vec<AutomationPoint>,
}

/// The state of one transport run's recording. Lives with the caller between
/// ticks; [`AutomationRecorder::finish`] ends the run.
#[derive(Debug, Default)]
pub struct AutomationRecorder {
    /// Keyed by `(track index or master, lane index)`, so a tick looks a pass
    /// up without allocating. A pass carries its lane's ids too: a key whose
    /// lane moved (a track added or removed mid-run) starts a fresh pass.
    passes: HashMap<(Option<usize>, usize), LanePass>,
    /// Each track's lanes from before the run first wrote to it: one undo
    /// step per track for the whole run.
    before: HashMap<String, Vec<AutomationLaneState>>,
    /// This tick's lanes and control values, as `(track index or master,
    /// lane index, value)`. Kept between ticks so a tick does not allocate.
    work: Vec<(Option<usize>, usize, f32)>,
}

/// What one tick did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecordTick {
    /// A lane started or stopped writing: the engine has to hear about it.
    pub engine_changed: bool,
    /// Points changed: the lanes repaint.
    pub wrote: bool,
}

impl AutomationRecorder {
    /// Whether nothing is being recorded or waiting to be committed.
    pub fn is_idle(&self) -> bool {
        self.passes.is_empty() && self.before.is_empty()
    }

    /// End the run at `beat`: close every writing lane, clear the recording
    /// marks, and hand back each written track's lanes from before the run,
    /// for its undo step. The second value says whether the engine has to
    /// resync (a lane stopped being held back).
    pub fn finish(
        &mut self,
        state: &mut TimelineState,
        beat: f32,
    ) -> (Vec<(String, Vec<AutomationLaneState>)>, bool) {
        let mut engine_changed = false;
        for (_, pass) in self.passes.drain() {
            let Some(lane) = state.lane_mut(&pass.track_id, &pass.lane_id) else {
                continue;
            };
            if pass.writing {
                write_value(lane, beat, pass.last_value);
            }
            if lane.recording {
                lane.recording = false;
                engine_changed = true;
            }
        }
        // Backstop: no lane outlives the run marked as being recorded.
        for lane in state
            .tracks
            .iter_mut()
            .flat_map(|t| t.automation_lanes.iter_mut())
            .chain(state.master.automation_lanes.iter_mut())
        {
            if lane.recording {
                lane.recording = false;
                engine_changed = true;
            }
        }
        (self.before.drain().collect(), engine_changed)
    }
}

/// The value of an insert's bypass switch or one of its parameters.
fn insert_control_value(inserts: &[InsertSlotState], target: &AutomationTarget) -> Option<f32> {
    match target {
        AutomationTarget::InsertBypass { insert_id } => inserts
            .iter()
            .find(|insert| insert.id == *insert_id)
            .map(|insert| if insert.bypassed { 1.0 } else { 0.0 }),
        AutomationTarget::PluginParameter {
            insert_id,
            parameter_id,
            ..
        } => inserts
            .iter()
            .find(|insert| insert.id == *insert_id)?
            .parameters
            .iter()
            .find(|param| param.id.to_string() == *parameter_id)
            .map(|param| param.value_normalized.clamp(0.0, 1.0)),
        _ => None,
    }
}

/// Replace the points after the last one up to `beat` and end on `value`.
fn write_value(lane: &mut AutomationLaneState, beat: f32, value: f32) {
    lane.points
        .retain(|p| p.beat < beat - PUNCH_GAP_BEATS * 0.5 || p.beat > beat);
    lane.points.push(AutomationPoint::new(beat, value));
    lane.sort_points();
}

impl TimelineState {
    /// The normalized value `target`'s control has on `track` now: the base
    /// value the user moves, never what automation is playing.
    pub fn automation_control_value(track: &TrackState, target: &AutomationTarget) -> Option<f32> {
        let switch = |on: bool| if on { 1.0 } else { 0.0 };
        match target {
            AutomationTarget::TrackVolume => Some(track.volume.clamp(0.0, 1.0)),
            AutomationTarget::TrackPan => Some(((track.pan + 1.0) * 0.5).clamp(0.0, 1.0)),
            AutomationTarget::TrackMute => Some(switch(track.muted)),
            AutomationTarget::SendLevel { send_id } => track
                .sends
                .iter()
                .find(|send| send.id == *send_id)
                .map(|send| volume::db_to_norm(send.gain_db)),
            AutomationTarget::SendEnabled { send_id } => track
                .sends
                .iter()
                .find(|send| send.id == *send_id)
                .map(|send| switch(send.enabled)),
            AutomationTarget::InsertBypass { .. } | AutomationTarget::PluginParameter { .. } => {
                insert_control_value(&track.inserts, target)
            }
        }
    }

    /// [`Self::automation_control_value`] for the master bus: its volume, and
    /// its inserts' bypass and parameters.
    pub fn master_automation_control_value(&self, target: &AutomationTarget) -> Option<f32> {
        match target {
            AutomationTarget::TrackVolume => Some(self.master.volume.clamp(0.0, 1.0)),
            AutomationTarget::InsertBypass { .. } | AutomationTarget::PluginParameter { .. } => {
                insert_control_value(&self.master.inserts, target)
            }
            _ => None,
        }
    }

    /// One playback tick of recording at `beat`, `now` seconds on any
    /// monotonic clock. Call only while the transport plays; does nothing
    /// unless the Automation Arm is on.
    pub fn automation_record_tick(
        &mut self,
        recorder: &mut AutomationRecorder,
        beat: f32,
        now: f64,
    ) -> RecordTick {
        let mut tick = RecordTick::default();
        if !self.automation_arm {
            return tick;
        }
        // Which lanes can record, and what their controls read, before
        // anything is borrowed mutably. Indices, not ids: nothing changes the
        // track or lane lists during a tick.
        let mut work = std::mem::take(&mut recorder.work);
        work.clear();
        for (track_ix, track) in self.tracks.iter().enumerate() {
            for (lane_ix, lane) in track.automation_lanes.iter().enumerate() {
                if !lane.enabled {
                    continue;
                }
                if let Some(value) = Self::automation_control_value(track, &lane.target) {
                    // A fader mid-drag holds its value in the preview until
                    // the drag commits; that is what is being heard.
                    let value = match (&lane.target, self.track_volume_previews.get(&track.id)) {
                        (AutomationTarget::TrackVolume, Some(preview)) => *preview,
                        _ => value,
                    };
                    work.push((Some(track_ix), lane_ix, value));
                }
            }
        }
        for (lane_ix, lane) in self.master.automation_lanes.iter().enumerate() {
            if !lane.enabled {
                continue;
            }
            if let Some(value) = self.master_automation_control_value(&lane.target) {
                let value = match (&lane.target, self.master_volume_preview) {
                    (AutomationTarget::TrackVolume, Some(preview)) => preview,
                    _ => value,
                };
                work.push((None, lane_ix, value));
            }
        }
        // Passes whose key now names another lane (a track moved or went away
        // mid-run). Rare, so this only allocates when it happens.
        let mut retired: Vec<LanePass> = Vec::new();
        for &(track_ix, lane_ix, value) in &work {
            let lanes = match track_ix {
                Some(ix) => &self.tracks[ix].automation_lanes,
                None => &self.master.automation_lanes,
            };
            let lane = &lanes[lane_ix];
            let track_id = match track_ix {
                Some(ix) => self.tracks[ix].id.as_str(),
                None => MASTER_TRACK_ID,
            };
            let key = (track_ix, lane_ix);
            let known = recorder
                .passes
                .get(&key)
                .is_some_and(|pass| pass.lane_id == lane.id && pass.track_id == track_id);
            // The first tick of a run only learns where each control stands.
            let Some(pass) = recorder.passes.get_mut(&key).filter(|_| known) else {
                if let Some(stale) = recorder.passes.remove(&key) {
                    if stale.writing {
                        retired.push(stale);
                    }
                }
                recorder.passes.insert(
                    key,
                    LanePass {
                        track_id: track_id.to_string(),
                        lane_id: lane.id.clone(),
                        last_value: value,
                        last_change: f64::NEG_INFINITY,
                        writing: false,
                        written_to: beat,
                        last_point: None,
                        original: lane.points.clone(),
                    },
                );
                continue;
            };
            let changed = (value - pass.last_value).abs() > VALUE_EPS;
            if changed {
                pass.last_change = now;
            }
            // The transport jumped back (a loop, a locate): writing carries on
            // from the new place rather than erasing backwards.
            if beat + PUNCH_GAP_BEATS < pass.written_to {
                pass.written_to = beat;
                pass.last_point = None;
            }
            let held = now - pass.last_change < TOUCH_RELEASE_SECONDS;
            let should_write = changed || (pass.writing && held);
            if !should_write && !pass.writing {
                pass.last_value = value;
                continue;
            }
            let track_id = pass.track_id.clone();
            // Taken only when the run first writes to the track: its undo step
            // has to put the lanes back as they were before any of it.
            let lanes_now =
                (should_write && !pass.writing && !recorder.before.contains_key(&track_id))
                    .then(|| self.capture_automation_lanes(&track_id));
            let lane = match track_ix {
                Some(ix) => &mut self.tracks[ix].automation_lanes[lane_ix],
                None => &mut self.master.automation_lanes[lane_ix],
            };
            let default = lane.target.default_value();

            if should_write && !pass.writing {
                // Punch in: the lane's own value up to here, then the control.
                // Writing a lane puts an overridden one back in play.
                lane.overridden = false;
                if let Some(lanes_now) = lanes_now {
                    recorder.before.entry(track_id.clone()).or_insert(lanes_now);
                }
                let lane_value = evaluate_automation(&pass.original, beat as f64, default);
                lane.points
                    .retain(|p| p.beat < beat || p.beat > beat + PUNCH_GAP_BEATS);
                if !lane.points.is_empty() || (lane_value - value).abs() > VALUE_EPS {
                    lane.points.push(AutomationPoint::new(beat, lane_value));
                }
                let point = AutomationPoint::new(beat + PUNCH_GAP_BEATS, value);
                pass.last_point = Some((point.id, value, false));
                lane.points.push(point);
                lane.sort_points();
                pass.writing = true;
                pass.written_to = beat + PUNCH_GAP_BEATS;
                lane.recording = true;
                tick.engine_changed = true;
                tick.wrote = true;
            } else if !should_write && pass.writing {
                // Touch release: the lane takes over again from here.
                let lane_value = evaluate_automation(&pass.original, beat as f64, default);
                let from = pass.written_to;
                lane.points
                    .retain(|p| p.beat <= from || p.beat > beat + PUNCH_GAP_BEATS);
                lane.points
                    .push(AutomationPoint::new(beat, pass.last_value));
                lane.points
                    .push(AutomationPoint::new(beat + PUNCH_GAP_BEATS, lane_value));
                lane.sort_points();
                pass.writing = false;
                pass.last_point = None;
                pass.written_to = beat + PUNCH_GAP_BEATS;
                lane.recording = false;
                tick.engine_changed = true;
                tick.wrote = true;
            } else if pass.writing && beat > pass.written_to {
                // Overwrite what the lane had from where writing reached.
                let from = pass.written_to;
                let keep_id = pass.last_point.map(|(id, _, _)| id);
                lane.points
                    .retain(|p| Some(p.id) == keep_id || p.beat <= from || p.beat > beat);
                match pass.last_point {
                    // A flat stretch: move its end rather than add a point.
                    Some((id, last_value, true)) if (value - last_value).abs() <= VALUE_EPS => {
                        if let Some(point) = lane.points.iter_mut().find(|p| p.id == id) {
                            point.beat = beat;
                        }
                    }
                    last => {
                        let flat = last.is_some_and(|(_, last_value, _)| {
                            (value - last_value).abs() <= VALUE_EPS
                        });
                        let point = AutomationPoint::new(beat, value);
                        pass.last_point = Some((point.id, value, flat));
                        lane.points.push(point);
                    }
                }
                lane.sort_points();
                pass.written_to = beat;
                tick.wrote = true;
            }
            pass.last_value = value;
        }
        recorder.work = work;
        // A retired pass that was writing closes where its lane now is: its
        // last value is kept and the lane plays again, instead of staying held
        // back from the engine for the rest of the session.
        for pass in retired {
            if let Some(lane) = self.lane_mut(&pass.track_id, &pass.lane_id) {
                write_value(lane, beat, pass.last_value);
                lane.recording = false;
                tick.engine_changed = true;
                tick.wrote = true;
            }
        }
        if tick.wrote {
            self.recompute_effective_volumes(beat, "automation_record");
        }
        tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A track with a Volume lane, the Automation Arm on and the transport
    /// rolling.
    fn armed_track_with_lane() -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let lane = state
            .ensure_automation_lane(&track, AutomationTarget::TrackVolume)
            .unwrap();
        state.automation_arm = true;
        state.transport.playing = true;
        (state, track, lane)
    }

    fn set_volume(state: &mut TimelineState, track: &str, volume: f32) {
        state
            .tracks
            .iter_mut()
            .find(|t| t.id == track)
            .unwrap()
            .volume = volume;
    }

    fn points(state: &TimelineState, track: &str, lane: &str) -> Vec<(f32, f32)> {
        state
            .automation_lane(track, lane)
            .unwrap()
            .points
            .iter()
            .map(|p| (p.beat, p.value))
            .collect()
    }

    /// The master's Volume records like a track's, from its own fader.
    #[test]
    fn master_volume_records_when_armed() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let lane = state
            .ensure_automation_lane(MASTER_TRACK_ID, AutomationTarget::TrackVolume)
            .unwrap();
        state.automation_arm = true;
        state.transport.playing = true;
        let mut recorder = AutomationRecorder::default();
        state.master.volume = 0.4;
        let first = state.automation_record_tick(&mut recorder, 0.0, 0.0);
        assert!(!first.wrote, "the first tick only learns where it stands");
        state.master.volume = 0.9;
        assert!(state.automation_record_tick(&mut recorder, 2.0, 0.2).wrote);
        let (before, _) = recorder.finish(&mut state, 3.0);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].0, MASTER_TRACK_ID);
        let recorded = points(&state, MASTER_TRACK_ID, &lane);
        assert_eq!(recorded.last().map(|&(_, v)| v), Some(0.9));
        assert!(
            !state
                .automation_lane(MASTER_TRACK_ID, &lane)
                .unwrap()
                .recording
        );
    }

    /// Armed, a moved control writes its lane, holds it back from the engine
    /// while it does, and hands back to the lane once let go: the curve
    /// after the release is the old one.
    #[test]
    fn a_touch_writes_then_hands_back_to_the_lane() {
        let (mut state, track, lane) = armed_track_with_lane();
        for (beat, value) in [(0.0, 0.2), (16.0, 0.2)] {
            state.add_automation_point(&track, &lane, beat, value);
        }
        set_volume(&mut state, &track, 0.2);
        let mut recorder = AutomationRecorder::default();
        let idle = state.automation_record_tick(&mut recorder, 1.0, 0.0);
        assert!(!idle.wrote, "an untouched control records nothing");
        set_volume(&mut state, &track, 0.9);
        let grab = state.automation_record_tick(&mut recorder, 2.0, 0.1);
        assert!(grab.wrote && grab.engine_changed);
        assert!(state.automation_lane(&track, &lane).unwrap().recording);
        state.automation_record_tick(&mut recorder, 3.0, 0.3);
        // Still for longer than the release time: back to the lane.
        let released = state.automation_record_tick(&mut recorder, 5.0, 1.0);
        assert!(released.engine_changed);
        assert!(!state.automation_lane(&track, &lane).unwrap().recording);
        let lane_state = state.automation_lane(&track, &lane).unwrap();
        let after = evaluate_automation(&lane_state.points, 8.0, 0.0);
        assert!((after - 0.2).abs() < 1e-4, "the old curve resumes: {after}");
        let during = evaluate_automation(&lane_state.points, 3.0, 0.0);
        assert!(
            (during - 0.9).abs() < 1e-4,
            "the touch was recorded: {during}"
        );
        let (undo, _) = recorder.finish(&mut state, 9.0);
        assert_eq!(undo.len(), 1, "one undo step for the run");
        assert!(recorder.is_idle());
    }

    /// Disarmed, nothing is written however the control moves.
    #[test]
    fn a_disarmed_transport_writes_nothing() {
        let (mut state, track, lane) = armed_track_with_lane();
        state.automation_arm = false;
        let mut recorder = AutomationRecorder::default();
        set_volume(&mut state, &track, 0.1);
        state.automation_record_tick(&mut recorder, 0.0, 0.0);
        set_volume(&mut state, &track, 0.9);
        let tick = state.automation_record_tick(&mut recorder, 1.0, 0.1);
        assert_eq!(tick, RecordTick::default());
        assert!(points(&state, &track, &lane).is_empty());
        assert!(recorder.is_idle());
    }

    /// A switched-off lane is never written.
    #[test]
    fn an_off_lane_is_left_alone() {
        let (mut state, track, lane) = armed_track_with_lane();
        state.set_automation_lane_enabled(&track, &lane, false);
        let mut recorder = AutomationRecorder::default();
        set_volume(&mut state, &track, 0.1);
        state.automation_record_tick(&mut recorder, 0.0, 0.0);
        set_volume(&mut state, &track, 0.9);
        assert!(!state.automation_record_tick(&mut recorder, 1.0, 0.1).wrote);
        assert!(points(&state, &track, &lane).is_empty());
    }

    /// Moving an automated control by hand while not recording overrides
    /// its lane; Re-Enable Automation puts it back. While recording, the
    /// same move is written instead, and writing clears an override.
    #[test]
    fn a_manual_move_overrides_until_re_enabled_and_recording_clears_it() {
        let (mut state, track, lane) = armed_track_with_lane();
        state.add_automation_point(&track, &lane, 0.0, 0.3);
        state.automation_arm = false;
        state.transport.playing = false;

        state.set_track_volume(&track, 0.8);
        let lane_state = state.automation_lane(&track, &lane).unwrap();
        assert!(lane_state.overridden && !lane_state.plays());
        assert!(state.has_overridden_automation());
        assert!(state.take_automation_override_change());
        assert!(!state.take_automation_override_change(), "taken once");
        assert!(
            (state.find_track(&track).unwrap().display_volume() - 0.8).abs() < 1e-6,
            "the fader shows what is heard: the hand-set value"
        );

        assert!(state.reenable_automation());
        assert!(!state.has_overridden_automation());
        assert!(state.take_automation_override_change());

        // Armed and rolling, the same move is a recording, not an override.
        state.automation_arm = true;
        state.transport.playing = true;
        assert!(!state.override_automation(&track, |_| true));
        let mut recorder = AutomationRecorder::default();
        state.automation_record_tick(&mut recorder, 1.0, 0.0);
        state.lane_mut(&track, &lane).unwrap().overridden = true;
        set_volume(&mut state, &track, 0.5);
        state.automation_record_tick(&mut recorder, 2.0, 0.1);
        assert!(
            !state.automation_lane(&track, &lane).unwrap().overridden,
            "writing a lane puts it back in play"
        );
    }

    /// Grabbing a fader without moving it is not a move: nothing is
    /// overridden. A value away from the base is.
    #[test]
    fn a_grab_without_a_move_does_not_override() {
        let (mut state, track, lane) = armed_track_with_lane();
        state.add_automation_point(&track, &lane, 0.0, 0.3);
        state.automation_arm = false;
        state.transport.playing = false;
        set_volume(&mut state, &track, 0.5);
        state.begin_track_volume_preview(&track, 0.5);
        assert!(!state.set_track_volume_preview(&track, 0.5));
        state.commit_track_volume_preview(&track);
        assert!(!state.automation_lane(&track, &lane).unwrap().overridden);
        state.set_track_pan(&track, 0.0);
        assert!(!state.has_overridden_automation());

        state.begin_track_volume_preview(&track, 0.5);
        state.set_track_volume_preview(&track, 0.6);
        assert!(state.automation_lane(&track, &lane).unwrap().overridden);
    }

    /// A lane being written whose track moves mid-run is closed, not left
    /// held back from the engine.
    #[test]
    fn a_lane_written_when_its_track_moves_is_closed() {
        let (mut state, first, lane) = armed_track_with_lane();
        let second = state.create_audio_track();
        state
            .ensure_automation_lane(&second, AutomationTarget::TrackVolume)
            .unwrap();
        let mut recorder = AutomationRecorder::default();
        set_volume(&mut state, &first, 0.2);
        state.automation_record_tick(&mut recorder, 0.0, 0.0);
        set_volume(&mut state, &first, 0.7);
        state.automation_record_tick(&mut recorder, 1.0, 0.1);
        assert!(state.automation_lane(&first, &lane).unwrap().recording);
        state.tracks.swap(0, 1);
        let tick = state.automation_record_tick(&mut recorder, 2.0, 0.2);
        assert!(tick.engine_changed);
        assert!(
            !state.automation_lane(&first, &lane).unwrap().recording,
            "the moved lane plays again"
        );
        recorder.finish(&mut state, 3.0);
        assert!(state
            .tracks
            .iter()
            .flat_map(|t| t.automation_lanes.iter())
            .all(|l| !l.recording));
    }

    /// Undo carries the current override across instead of the captured one.
    #[test]
    fn undo_keeps_the_current_override() {
        let (mut state, track, lane) = armed_track_with_lane();
        state.add_automation_point(&track, &lane, 0.0, 0.3);
        state.automation_arm = false;
        state.transport.playing = false;
        let before = state.capture_automation_lanes(&track);
        state.set_track_volume(&track, 0.9);
        assert!(state.automation_lane(&track, &lane).unwrap().overridden);
        state.set_track_automation_lanes(&track, before);
        assert!(
            state.automation_lane(&track, &lane).unwrap().overridden,
            "an undo does not silently re-enable"
        );
    }
}

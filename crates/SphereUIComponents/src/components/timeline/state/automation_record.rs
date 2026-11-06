//! Write, Touch and Latch: recording a control's movement into its lane
//! while the transport plays.
//!
//! Driven from the playback tick (UI thread, display rate), not the audio
//! thread. Each tick reads the control's own value — the base value the user
//! moves, never the automated one the fader displays — and, while a lane is
//! writing, replaces its points from where writing reached to the playhead.
//!
//! A touch is the control changing. Studio's controls (faders, pans, sends,
//! switches, plug-in editors) do not all report a held gesture, so a control
//! counts as touched while it moves and is released after
//! [`TOUCH_RELEASE_SECONDS`] of standing still. That covers every control the
//! same way, a plug-in's own editor included.
//!
//! While a lane writes it is marked `recording`, and the engine snapshot
//! leaves it out: what is heard is the moving control, and what is written is
//! what was heard. The whole run is one undo step per track.

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
    passes: HashMap<(String, String), LanePass>,
    /// Each track's lanes from before the run first wrote to it: one undo
    /// step per track for the whole run.
    before: HashMap<String, Vec<AutomationLaneState>>,
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
        for ((track_id, lane_id), pass) in self.passes.drain() {
            let Some(lane) = state.lane_mut(&track_id, &lane_id) else {
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
    /// monotonic clock. Call only while the transport plays.
    pub fn automation_record_tick(
        &mut self,
        recorder: &mut AutomationRecorder,
        beat: f32,
        now: f64,
    ) -> RecordTick {
        let mut tick = RecordTick::default();
        // Which lanes record, and what their controls read, before anything
        // is borrowed mutably.
        let mut work = Vec::new();
        for track in &self.tracks {
            for lane in &track.automation_lanes {
                if !lane.enabled || !lane.mode.records() {
                    continue;
                }
                let Some(value) = Self::automation_control_value(track, &lane.target) else {
                    continue;
                };
                work.push((track.id.clone(), lane.id.clone(), lane.mode, value));
            }
        }
        for lane in &self.master.automation_lanes {
            if !lane.enabled || !lane.mode.records() {
                continue;
            }
            let Some(value) = self.master_automation_control_value(&lane.target) else {
                continue;
            };
            work.push((
                MASTER_TRACK_ID.to_string(),
                lane.id.clone(),
                lane.mode,
                value,
            ));
        }
        for (track_id, lane_id, mode, value) in work {
            // Taken only until the run first writes to the track: its undo step
            // has to put the lanes back as they were before any of it.
            let lanes_now = (!recorder.before.contains_key(&track_id))
                .then(|| self.capture_automation_lanes(&track_id));
            let Some(lane) = self.lane_mut(&track_id, &lane_id) else {
                continue;
            };
            let default = lane.target.default_value();
            let pass = recorder
                .passes
                .entry((track_id.clone(), lane_id.clone()))
                .or_insert_with(|| LanePass {
                    last_value: value,
                    last_change: f64::NEG_INFINITY,
                    writing: false,
                    written_to: beat,
                    last_point: None,
                    original: lane.points.clone(),
                });
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
            let should_write = match mode {
                AutomationMode::Write => true,
                AutomationMode::Latch => pass.writing || changed,
                AutomationMode::Touch => changed || (pass.writing && held),
                AutomationMode::Read => false,
            };

            if should_write && !pass.writing {
                // Punch in: the lane's own value up to here, then the control.
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
        if tick.wrote {
            self.recompute_effective_volumes(beat, "automation_record");
        }
        tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_with_lane(mode: AutomationMode) -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let lane = state
            .ensure_automation_lane(&track, AutomationTarget::TrackVolume)
            .unwrap();
        state.lane_mut(&track, &lane).unwrap().mode = mode;
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
    fn master_volume_records_in_write() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let lane = state
            .ensure_automation_lane(MASTER_TRACK_ID, AutomationTarget::TrackVolume)
            .unwrap();
        state.lane_mut(MASTER_TRACK_ID, &lane).unwrap().mode = AutomationMode::Write;
        let mut recorder = AutomationRecorder::default();
        state.master.volume = 0.4;
        let first = state.automation_record_tick(&mut recorder, 0.0, 0.0);
        assert!(first.wrote);
        state.master.volume = 0.9;
        state.automation_record_tick(&mut recorder, 2.0, 0.2);
        let (before, _) = recorder.finish(&mut state, 3.0);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].0, MASTER_TRACK_ID);
        let recorded = points(&state, MASTER_TRACK_ID, &lane);
        assert!(recorded.iter().any(|&(_, v)| (v - 0.4).abs() < 1.0e-4));
        assert_eq!(recorded.last().map(|&(_, v)| v), Some(0.9));
        assert!(
            !state
                .automation_lane(MASTER_TRACK_ID, &lane)
                .unwrap()
                .recording
        );
    }

    /// Write records from the first tick, holds the lane back from the engine
    /// while it does, and ends on the control's value when the run finishes.
    #[test]
    fn write_records_the_whole_run() {
        let (mut state, track, lane) = track_with_lane(AutomationMode::Write);
        let mut recorder = AutomationRecorder::default();
        set_volume(&mut state, &track, 0.5);
        let first = state.automation_record_tick(&mut recorder, 0.0, 0.0);
        assert!(first.engine_changed && first.wrote);
        assert!(state.automation_lane(&track, &lane).unwrap().recording);
        state.automation_record_tick(&mut recorder, 1.0, 0.1);
        set_volume(&mut state, &track, 0.8);
        state.automation_record_tick(&mut recorder, 2.0, 0.2);
        let (undo, engine) = recorder.finish(&mut state, 3.0);
        assert!(engine && undo.len() == 1 && recorder.is_idle());
        let recorded = points(&state, &track, &lane);
        assert!((recorded.last().unwrap().1 - 0.8).abs() < 1e-6);
        assert!(recorded.iter().any(|(_, v)| (v - 0.5).abs() < 1e-6));
        assert!(!state.automation_lane(&track, &lane).unwrap().recording);
    }

    /// Touch records only while the control moves, then hands back to the
    /// lane: the curve after the release is the old one.
    #[test]
    fn touch_hands_back_to_the_lane_when_let_go() {
        let (mut state, track, lane) = track_with_lane(AutomationMode::Touch);
        for (beat, value) in [(0.0, 0.2), (16.0, 0.2)] {
            state.add_automation_point(&track, &lane, beat, value);
        }
        set_volume(&mut state, &track, 0.2);
        let mut recorder = AutomationRecorder::default();
        let idle = state.automation_record_tick(&mut recorder, 1.0, 0.0);
        assert!(!idle.wrote, "an untouched control records nothing");
        set_volume(&mut state, &track, 0.9);
        assert!(state.automation_record_tick(&mut recorder, 2.0, 0.1).wrote);
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
    }

    /// Latch keeps writing the last value after the control stops moving.
    #[test]
    fn latch_keeps_writing_after_the_control_stops() {
        let (mut state, track, lane) = track_with_lane(AutomationMode::Latch);
        set_volume(&mut state, &track, 0.3);
        let mut recorder = AutomationRecorder::default();
        state.automation_record_tick(&mut recorder, 0.0, 0.0);
        set_volume(&mut state, &track, 0.6);
        state.automation_record_tick(&mut recorder, 1.0, 0.1);
        state.automation_record_tick(&mut recorder, 6.0, 5.0);
        assert!(
            state.automation_lane(&track, &lane).unwrap().recording,
            "still latched"
        );
        recorder.finish(&mut state, 8.0);
        let lane_state = state.automation_lane(&track, &lane).unwrap();
        assert!((evaluate_automation(&lane_state.points, 7.0, 0.0) - 0.6).abs() < 1e-4);
        // A flat stretch is a couple of points, not one per tick.
        assert!(
            lane_state.points.len() <= 5,
            "{:?}",
            points(&state, &track, &lane)
        );
    }

    /// Read and Off lanes are never written.
    #[test]
    fn read_lanes_are_left_alone() {
        let (mut state, track, lane) = track_with_lane(AutomationMode::Read);
        let mut recorder = AutomationRecorder::default();
        set_volume(&mut state, &track, 0.1);
        state.automation_record_tick(&mut recorder, 0.0, 0.0);
        set_volume(&mut state, &track, 0.9);
        let tick = state.automation_record_tick(&mut recorder, 1.0, 0.1);
        assert_eq!(tick, RecordTick::default());
        assert!(points(&state, &track, &lane).is_empty());
        assert!(recorder.is_idle());
    }
}

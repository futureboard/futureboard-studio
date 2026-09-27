//! Editing a multi-selection of tracks together.
//!
//! A control pressed on a track that is one of several selected tracks acts
//! on all of them: a toggle sets every one to the pressed track's new state,
//! and a fader or pan drag moves every one by the same amount the pressed
//! track moves (in dB for volume), each from where it started. A control on a
//! track outside the selection acts on that track alone. The header, the
//! mixer and the inspector all ask [`TimelineState::gang_targets`], so the
//! three surfaces cannot disagree about what a press reaches.

use super::*;

impl TimelineState {
    /// The tracks an edit of `track_id` reaches: the whole selection when the
    /// track is one of several selected tracks, otherwise the track itself.
    pub fn gang_targets(&self, track_id: &str) -> Vec<String> {
        let selected = self.selected_range_track_ids();
        if selected.len() > 1 && selected.iter().any(|id| id == track_id) {
            // Master and mixer-only VSTi channels can be selected in the
            // mixer; only real tracks take a ganged edit.
            selected
                .into_iter()
                .filter(|id| self.find_track(id).is_some())
                .collect()
        } else {
            vec![track_id.to_string()]
        }
    }

    /// A press on `track_id` that may land on one of its controls. Without
    /// Ctrl or Shift, a press on a track already in a multi-selection keeps
    /// the selection (the track becomes its primary), so the control reaches
    /// every selected track; anything else selects the way a click does.
    pub fn press_track_selection(&mut self, track_id: &str, additive: bool, range: bool) {
        let keeps = !additive
            && !range
            && self.selection.is_track_selected(track_id)
            && self.selected_range_track_ids().len() > 1;
        if keeps {
            self.selection.selected_track_id = Some(track_id.to_string());
        } else {
            self.select_track_with_modifiers(track_id, additive, range);
        }
    }

    /// Starts a fader drag on `track_id` at `norm`, and on every track it is
    /// ganged with at that track's own current level.
    pub fn begin_volume_gang(&mut self, track_id: &str, norm: f32) {
        for target in self.gang_targets(track_id) {
            let start = if target == track_id {
                norm
            } else {
                self.display_track_volume_of(&target)
            };
            self.begin_track_volume_preview(&target, start);
        }
    }

    /// Moves a ganged fader drag: `track_id` to `norm`, every other track in
    /// the gang by the same change from where it started. Returns the tracks
    /// whose level changed, with their new level, for the live engine path.
    pub fn set_volume_gang_preview(&mut self, track_id: &str, norm: f32) -> Vec<(String, f32)> {
        let Some(grab_origin) = self.track_volume_gesture_origin.get(track_id).copied() else {
            return Vec::new();
        };
        let delta = norm.clamp(0.0, 1.0) - grab_origin;
        let mut changed = Vec::new();
        for target in self.gang_targets(track_id) {
            let Some(origin) = self.track_volume_gesture_origin.get(&target).copied() else {
                continue;
            };
            // The fader scale is linear in dB, so one offset in it is one
            // offset in dB for every track.
            let next = if target == track_id {
                norm
            } else {
                (origin + delta).clamp(0.0, 1.0)
            };
            if self.set_track_volume_preview(&target, next) {
                changed.push((target, next.clamp(0.0, 1.0)));
            }
        }
        changed
    }

    /// Ends a ganged fader drag: every track's preview becomes its level.
    /// Returns `(track, before, after)` for each.
    pub fn commit_volume_gang(&mut self, track_id: &str) -> Vec<(String, f32, f32)> {
        self.gang_targets(track_id)
            .into_iter()
            .filter_map(|target| {
                let (prev, next) = self.commit_track_volume_preview(&target)?;
                Some((target, prev, next))
            })
            .collect()
    }

    /// Starts a pan drag on `track_id` and every track ganged with it.
    pub fn begin_pan_gang(&mut self, track_id: &str) {
        for target in self.gang_targets(track_id) {
            self.begin_track_pan_preview(&target);
        }
    }

    /// Moves a ganged pan drag: `track_id` to `pan`, every other track by the
    /// same change from where it started. Returns the tracks whose pan
    /// changed, with their new pan.
    pub fn set_pan_gang_preview(&mut self, track_id: &str, pan: f32) -> Vec<(String, f32)> {
        let Some(grab_origin) = self.track_pan_gesture_origin.get(track_id).copied() else {
            return Vec::new();
        };
        let delta = pan.clamp(-1.0, 1.0) - grab_origin;
        let mut changed = Vec::new();
        for target in self.gang_targets(track_id) {
            let Some(origin) = self.track_pan_gesture_origin.get(&target).copied() else {
                continue;
            };
            let next = if target == track_id {
                pan.clamp(-1.0, 1.0)
            } else {
                (origin + delta).clamp(-1.0, 1.0)
            };
            if self.set_track_pan_preview(&target, next) {
                changed.push((target, next));
            }
        }
        changed
    }

    /// Ends a ganged pan drag. Returns `(track, before, after)` for each.
    pub fn commit_pan_gang(&mut self, track_id: &str) -> Vec<(String, f32, f32)> {
        self.gang_targets(track_id)
            .into_iter()
            .filter_map(|target| {
                let (prev, next) = self.commit_track_pan_preview(&target)?;
                Some((target, prev, next))
            })
            .collect()
    }

    pub fn set_track_armed(&mut self, track_id: &str, armed: bool) -> bool {
        match self.tracks.iter_mut().find(|track| track.id == track_id) {
            Some(track) if track.armed != armed => {
                track.armed = armed;
                true
            }
            _ => false,
        }
    }

    pub fn set_track_input_monitor(&mut self, track_id: &str, mode: InputMonitorMode) -> bool {
        match self.tracks.iter_mut().find(|track| track.id == track_id) {
            Some(track) if track.input_monitor != mode => {
                track.input_monitor = mode;
                true
            }
            _ => false,
        }
    }

    /// The level a track's fader shows now (a drag's preview when one runs).
    fn display_track_volume_of(&self, track_id: &str) -> f32 {
        self.find_track(track_id)
            .map(|track| self.display_track_volume(track))
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_tracks() -> (TimelineState, Vec<String>) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let ids: Vec<String> = (0..3).map(|_| state.create_audio_track()).collect();
        (state, ids)
    }

    #[test]
    fn a_press_inside_the_selection_reaches_all_of_it() {
        let (mut state, ids) = three_tracks();
        state.select_track(&ids[0]);
        state.selection.selected_track_ids = vec![ids[0].clone(), ids[1].clone()];
        assert_eq!(
            state.gang_targets(&ids[1]),
            [ids[0].clone(), ids[1].clone()]
        );
        // Outside the selection: just that track.
        assert_eq!(state.gang_targets(&ids[2]), [ids[2].clone()]);
    }

    #[test]
    fn a_ganged_fader_moves_every_track_by_the_same_db() {
        let (mut state, ids) = three_tracks();
        state.set_track_volume(&ids[0], 0.5);
        state.set_track_volume(&ids[1], 0.8);
        state.select_track(&ids[0]);
        state.selection.selected_track_ids = vec![ids[0].clone(), ids[1].clone()];

        state.begin_volume_gang(&ids[0], 0.5);
        let moved = state.set_volume_gang_preview(&ids[0], 0.6);
        assert_eq!(moved.len(), 2);
        // Levels do not change until the drag ends.
        assert!((state.find_track(&ids[1]).unwrap().volume - 0.8).abs() < 1e-6);
        let committed = state.commit_volume_gang(&ids[0]);
        assert_eq!(committed.len(), 2);
        assert!((state.find_track(&ids[0]).unwrap().volume - 0.6).abs() < 1e-5);
        assert!((state.find_track(&ids[1]).unwrap().volume - 0.9).abs() < 1e-5);
        // The unselected track is untouched.
        assert!(state.find_track(&ids[2]).unwrap().volume > 0.0);
        assert!(state.track_volume_previews.is_empty());
    }

    #[test]
    fn a_ganged_pan_keeps_each_tracks_offset_and_clamps() {
        let (mut state, ids) = three_tracks();
        state.set_track_pan(&ids[0], 0.0);
        state.set_track_pan(&ids[1], 0.8);
        state.select_track(&ids[0]);
        state.selection.selected_track_ids = vec![ids[0].clone(), ids[1].clone()];

        state.begin_pan_gang(&ids[0]);
        state.set_pan_gang_preview(&ids[0], 0.5);
        assert!((state.find_track(&ids[1]).unwrap().pan - 1.0).abs() < 1e-6);
        let committed = state.commit_pan_gang(&ids[0]);
        assert_eq!(committed.len(), 2);
        assert!((committed[1].1 - 0.8).abs() < 1e-6);
    }
}

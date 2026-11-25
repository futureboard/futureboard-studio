//! VCA groups: a fader that moves a set of channels' levels together without
//! any audio passing through it.
//!
//! A member keeps its own fader; the engine plays it at that fader times the
//! VCA's gain ([`TimelineState::vca_gain_for_track`]), automation included.
//! Muting the VCA silences its members. The groups live on the master bus
//! state, so the master's undo scope covers them; a track's membership is
//! its own setting, under its own scope. Creating and changing VCAs is
//! Professional mixing; every edition plays them.

use super::*;

/// One VCA group.
#[derive(Debug, Clone, PartialEq)]
pub struct VcaGroupState {
    pub id: String,
    pub name: String,
    pub color: gpui::Rgba,
    /// The VCA fader, on the channel faders' normalized scale
    /// ([`volume::norm_to_db`]): unity is `volume::db_to_norm(0.0)`.
    pub level: f32,
    pub muted: bool,
}

impl VcaGroupState {
    /// The linear gain the VCA puts on each member: 0 while muted or at the
    /// bottom of its travel, unity at 0 dB.
    pub fn gain(&self) -> f32 {
        if self.muted || self.level <= 0.001 {
            return 0.0;
        }
        10f32.powf(volume::norm_to_db(self.level) / 20.0)
    }
}

impl TimelineState {
    /// The VCA group `id`.
    pub fn vca_group(&self, id: &str) -> Option<&VcaGroupState> {
        self.master.vca_groups.iter().find(|group| group.id == id)
    }

    /// The linear gain `track`'s VCA puts on its fader; 1 outside a VCA, or
    /// in one that no longer exists.
    pub fn vca_gain_for_track(&self, track: &TrackState) -> f32 {
        track
            .vca_group
            .as_deref()
            .and_then(|id| self.vca_group(id))
            .map_or(1.0, VcaGroupState::gain)
    }

    /// The tracks in VCA `id`, in track order.
    pub fn vca_members(&self, id: &str) -> Vec<String> {
        self.tracks
            .iter()
            .filter(|track| track.vca_group.as_deref() == Some(id))
            .map(|track| track.id.clone())
            .collect()
    }

    /// A new VCA at unity named `name`, with `members` moved into it (out of
    /// any VCA they were in). Returns its id.
    pub fn create_vca_group(&mut self, name: &str, members: &[String]) -> String {
        let number = self.master.vca_groups.len() + 1;
        // Numbered past every VCA there is, so an id is never reused while
        // a member still names it.
        let id = (1..)
            .map(|n| format!("vca-{n}"))
            .find(|id| self.vca_group(id).is_none())
            .unwrap_or_default();
        let color = members
            .first()
            .and_then(|member| self.find_track(member))
            .map_or_else(|| self.track_color_for_index(number), |track| track.color);
        let name = if name.trim().is_empty() {
            format!("VCA {number}")
        } else {
            name.trim().to_string()
        };
        self.master.vca_groups.push(VcaGroupState {
            id: id.clone(),
            name,
            color,
            level: volume::db_to_norm(0.0),
            muted: false,
        });
        for member in members {
            self.set_track_vca_group(member, Some(&id));
        }
        id
    }

    /// Put `track_id` in VCA `group` (`None`: out of any). A VCA that does
    /// not exist, or the master, refuses. Returns whether it changed.
    pub fn set_track_vca_group(&mut self, track_id: &str, group: Option<&str>) -> bool {
        if let Some(id) = group {
            if self.vca_group(id).is_none() {
                return false;
            }
        }
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        if track.vca_group.as_deref() == group {
            return false;
        }
        track.vca_group = group.map(str::to_string);
        true
    }

    /// Move VCA `id`'s fader. Returns whether it moved.
    pub fn set_vca_level(&mut self, id: &str, level: f32) -> bool {
        let level = level.clamp(0.0, 1.0);
        match self
            .master
            .vca_groups
            .iter_mut()
            .find(|group| group.id == id)
        {
            Some(group) if (group.level - level).abs() > 1.0e-5 => {
                group.level = level;
                true
            }
            _ => false,
        }
    }

    /// Mute or unmute VCA `id`. Returns whether it changed.
    pub fn set_vca_muted(&mut self, id: &str, muted: bool) -> bool {
        match self
            .master
            .vca_groups
            .iter_mut()
            .find(|group| group.id == id)
        {
            Some(group) if group.muted != muted => {
                group.muted = muted;
                true
            }
            _ => false,
        }
    }

    /// Remove VCA `id`; its members go back to their own faders. Returns the
    /// members it had.
    pub fn delete_vca_group(&mut self, id: &str) -> Vec<String> {
        let members = self.vca_members(id);
        self.master.vca_groups.retain(|group| group.id != id);
        for track in &mut self.tracks {
            if track.vca_group.as_deref() == Some(id) {
                track.vca_group = None;
            }
        }
        members
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(names: &[&str]) -> (TimelineState, Vec<String>) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let ids = names
            .iter()
            .map(|name| {
                state.create_track(CreateTrackOptions {
                    name: name.to_string(),
                    track_type: TrackType::Audio,
                    color: gpui::rgb(0x808080),
                    volume: volume::db_to_norm(0.0),
                    pan: 0.0,
                    armed: false,
                    input_monitor: InputMonitorMode::Off,
                })
            })
            .collect();
        (state, ids)
    }

    #[test]
    fn a_vca_scales_its_members_and_mutes_them() {
        let (mut state, ids) = state_with(&["Kick", "Snare", "Bass"]);
        let vca = state.create_vca_group("Drums", &ids[..2]);
        assert_eq!(state.vca_members(&vca), ids[..2].to_vec());
        let gain = |state: &TimelineState, i: usize| {
            state.vca_gain_for_track(state.find_track(&ids[i]).unwrap())
        };
        assert!(
            (gain(&state, 0) - 1.0).abs() < 1e-6,
            "a new VCA is at unity"
        );

        assert!(state.set_vca_level(&vca, volume::db_to_norm(-6.0)));
        assert!((gain(&state, 0) - 10f32.powf(-6.0 / 20.0)).abs() < 1e-4);
        assert!((gain(&state, 2) - 1.0).abs() < 1e-6, "Bass is not a member");

        assert!(state.set_vca_muted(&vca, true));
        assert_eq!(gain(&state, 1), 0.0);

        assert_eq!(state.delete_vca_group(&vca), ids[..2].to_vec());
        assert!((gain(&state, 0) - 1.0).abs() < 1e-6);
        assert!(state.find_track(&ids[0]).unwrap().vca_group.is_none());
    }

    #[test]
    fn a_track_moves_between_vcas_but_not_into_a_missing_one() {
        let (mut state, ids) = state_with(&["Vox"]);
        let a = state.create_vca_group("", &ids);
        let b = state.create_vca_group("", &[]);
        assert_eq!(state.vca_group(&a).unwrap().name, "VCA 1");
        assert!(state.set_track_vca_group(&ids[0], Some(&b)));
        assert!(state.vca_members(&a).is_empty());
        assert!(!state.set_track_vca_group(&ids[0], Some("vca-gone")));
        assert!(state.set_track_vca_group(&ids[0], None));
        assert!(!state.set_track_vca_group(&ids[0], None), "no change");
    }
}

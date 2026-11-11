use super::*;

/// Origin of a track volume change, so the base/effective model can route the
/// write correctly and never let an automation-follow display update masquerade
/// as a user fader edit (which would fight automation / spam dirty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeUpdateSource {
    /// User dragged the mixer/track-header/inspector fader — edits base only.
    UserFader,
    /// Automation read at the playhead — edits effective only.
    AutomationRead,
    /// Project load / programmatic reset — sets base and effective together.
    ProjectLoad,
}

/// Volume / dB mapping helpers. Linear in dB between the soft floor and a
/// little headroom above unity.
pub mod volume {
    pub const MIN_DB: f32 = -60.0;
    pub const MAX_DB: f32 = 6.0;

    pub fn norm_to_db(norm: f32) -> f32 {
        let n = norm.clamp(0.0, 1.0);
        MIN_DB + n * (MAX_DB - MIN_DB)
    }

    pub fn db_to_norm(db: f32) -> f32 {
        ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0)
    }

    pub fn format_db(norm: f32) -> String {
        let db = norm_to_db(norm);
        if norm <= 0.001 || db <= MIN_DB + 0.05 {
            "-∞".to_string()
        } else if db >= 0.0 {
            format!("+{:.1}", db)
        } else {
            format!("{:.1}", db)
        }
    }
}

impl TimelineState {
    // ── Single-source-of-truth mutations ─────────────────────────────────────
    // These are the only paths that should mutate per-track UI state. Both the
    // timeline TrackHeader and the bottom-panel Mixer call into these, so the
    // two views can never drift apart.

    pub fn fader_debug_enabled() -> bool {
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_FADER_DEBUG").is_some())
    }

    pub fn display_master_volume(&self) -> f32 {
        self.master_volume_preview.unwrap_or(self.master.volume)
    }

    pub fn display_track_volume(&self, track: &TrackState) -> f32 {
        self.track_volume_previews
            .get(&track.id)
            .copied()
            .unwrap_or_else(|| track.display_volume())
    }

    pub fn set_master_volume(&mut self, norm: f32) {
        let v = norm.clamp(0.0, 1.0);
        if (self.master.volume - v).abs() > 1.0e-5 {
            self.override_automation(MASTER_TRACK_ID, is_volume);
        }
        self.master.volume = v;
    }

    /// Set the Control Room level. Unlike the master fader this needs no
    /// preview/commit pair: monitor level is session state, so there is no
    /// undo entry to coalesce and no project dirty flag to defer — every
    /// pointer sample is simply the new value.
    ///
    /// Returns `true` when the value actually moved, so the caller can skip a
    /// redundant engine store and repaint during a drag.
    pub fn set_monitor_volume(&mut self, norm: f32) -> bool {
        let v = norm.clamp(0.0, 1.0);
        let changed = (self.monitor.volume - v).abs() > 1.0e-5;
        if changed {
            self.monitor.volume = v;
        }
        changed
    }

    /// Toggle Control Room mute. Monitoring only — the master mix, exports,
    /// and recordings are unaffected.
    pub fn toggle_monitor_mute(&mut self) {
        self.monitor.mute = !self.monitor.mute;
    }

    /// Toggle the -20 dB Dim reference cut.
    pub fn toggle_monitor_dim(&mut self) {
        self.monitor.dim = !self.monitor.dim;
    }

    /// Toggle mono folding for mono-compatibility checking.
    pub fn toggle_monitor_mono(&mut self) {
        self.monitor.mono = !self.monitor.mono;
    }

    /// Select what the Control Room monitors. Returns `true` when it changed.
    ///
    /// Resolves the display name here, where the project is in scope, so the
    /// strip shows a bus's real name instead of its internal id.
    pub fn set_monitor_source(&mut self, source: MonitorSourceKind) -> bool {
        if self.monitor.source == source {
            return false;
        }
        self.monitor.source_display = match &source {
            MonitorSourceKind::MasterBus => "Master Bus".to_string(),
            MonitorSourceKind::Bus(id)
            | MonitorSourceKind::TrackPreFader(id)
            | MonitorSourceKind::TrackAfterFader(id) => self
                .find_track(id)
                .map(|track| track.name.clone())
                .unwrap_or_else(|| id.clone()),
            MonitorSourceKind::HardwareInput(id) => id.clone(),
        };
        self.monitor.source = source;
        true
    }

    /// Set one channel's Listen state, exclusively.
    ///
    /// Engaging PFL or AFL on a channel clears Listen everywhere else, so the
    /// Control Room is always on a single unambiguous tap — the console
    /// convention, and it keeps the listen bus from summing tracks the user did
    /// not intend to compare. Clicking the engaged mode again clears it and
    /// returns monitoring to the selected source.
    ///
    /// Returns `true` when anything changed.
    pub fn set_track_listen(&mut self, track_id: &str, mode: ListenMode) -> bool {
        let current = self
            .find_track(track_id)
            .map(|track| track.listen)
            .unwrap_or_default();
        let next = if current == mode {
            ListenMode::Off
        } else {
            mode
        };
        let mut changed = false;
        for track in &mut self.tracks {
            let want = if track.id == track_id {
                next
            } else {
                ListenMode::Off
            };
            if track.listen != want {
                track.listen = want;
                changed = true;
            }
        }
        if changed {
            self.monitor.listen_active = next.is_active();
        }
        changed
    }

    /// Clear Listen on every channel — monitoring returns to the Control
    /// Room's selected source.
    pub fn clear_all_listen(&mut self) -> bool {
        let mut changed = false;
        for track in &mut self.tracks {
            if track.listen != ListenMode::Off {
                track.listen = ListenMode::Off;
                changed = true;
            }
        }
        if self.monitor.listen_active {
            self.monitor.listen_active = false;
            changed = true;
        }
        changed
    }

    /// The channel currently soloed into the Control Room's listen bus, if any.
    pub fn active_listen_track(&self) -> Option<(&str, ListenMode)> {
        self.tracks
            .iter()
            .find(|track| track.listen.is_active())
            .map(|track| (track.id.as_str(), track.listen))
    }

    /// Sources the Control Room can be switched to, in cycle order.
    ///
    /// Master Bus is always first, so the default and the first step of the
    /// cycle are the complete internal mix — never a hardware input. Project
    /// routing buses (group / aux / return) follow, tapped post-fader.
    pub fn monitor_source_options(&self) -> Vec<MonitorSourceKind> {
        let mut options = vec![MonitorSourceKind::MasterBus];
        options.extend(
            self.tracks
                .iter()
                .filter(|track| {
                    matches!(
                        track.track_type,
                        TrackType::Group | TrackType::Bus | TrackType::Return
                    )
                })
                .map(|track| MonitorSourceKind::Bus(track.id.clone())),
        );
        options
    }

    /// Re-resolve the Master and Monitor Output chip labels from the registry.
    ///
    /// Display only — neither `master_output_connection_id` nor
    /// `monitor_output_connection_id` is touched, which is why renaming a bus
    /// updates both strips without disturbing any routing. Call after a
    /// registry edit, an output assignment, or a project load.
    pub fn refresh_output_labels(&mut self) -> bool {
        let master = crate::output_routing::master_output_label(
            &self.audio_connections,
            self.master_output_connection_id.as_ref(),
        );
        let monitor = crate::output_routing::monitor_output_label(
            &self.audio_connections,
            self.master_output_connection_id.as_ref(),
            self.monitor_output_connection_id.as_ref(),
        );
        let changed = self.master.output_label != master || self.monitor.output_name != monitor;
        self.master.output_label = master;
        self.monitor.output_name = monitor;
        changed
    }

    pub fn begin_master_volume_preview(&mut self, norm: f32) {
        self.master_volume_preview = Some(norm.clamp(0.0, 1.0));
        if Self::fader_debug_enabled() {
            eprintln!(
                "[fader] drag start target=master norm={:.4}",
                norm.clamp(0.0, 1.0)
            );
        }
    }

    pub fn set_master_volume_preview(&mut self, norm: f32) -> bool {
        let v = norm.clamp(0.0, 1.0);
        if (self.master.volume - v).abs() > 1.0e-5 {
            self.override_automation(MASTER_TRACK_ID, is_volume);
        }
        let changed = self
            .master_volume_preview
            .map(|prev| (prev - v).abs() > 1.0e-5)
            .unwrap_or(true);
        if changed {
            self.master_volume_preview = Some(v);
        }
        changed
    }

    pub fn commit_master_volume_preview(&mut self) -> Option<f32> {
        let v = self.master_volume_preview.take()?;
        self.set_master_volume(v);
        if Self::fader_debug_enabled() {
            eprintln!("[fader] commit target=master norm={v:.4}");
        }
        Some(v)
    }

    pub fn begin_track_volume_preview(&mut self, track_id: &str, norm: f32) {
        let v = norm.clamp(0.0, 1.0);
        if !self.track_volume_gesture_origin.contains_key(track_id) {
            let origin = self
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .map(|t| t.volume)
                .unwrap_or(v);
            self.track_volume_gesture_origin
                .insert(track_id.to_string(), origin);
        }
        self.track_volume_previews.insert(track_id.to_string(), v);
        if Self::fader_debug_enabled() {
            eprintln!("[fader] drag start track={track_id} norm={v:.4}");
        }
    }

    pub fn set_track_volume_preview(&mut self, track_id: &str, norm: f32) -> bool {
        let v = norm.clamp(0.0, 1.0);
        // Grabbing a fader is not moving it: only a value away from the base
        // overrides the lane.
        if self
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .is_some_and(|t| (t.volume - v).abs() > 1.0e-5)
        {
            self.override_automation(track_id, is_volume);
        }
        let changed = self
            .track_volume_previews
            .get(track_id)
            .map(|prev| (*prev - v).abs() > 1.0e-5)
            .unwrap_or(true);
        if changed {
            self.track_volume_previews.insert(track_id.to_string(), v);
        }
        changed
    }

    /// Commit a fader drag. Returns `(prev, next)` when a preview existed so
    /// the caller can record one undo entry for the whole gesture.
    pub fn commit_track_volume_preview(&mut self, track_id: &str) -> Option<(f32, f32)> {
        let next = self.track_volume_previews.remove(track_id)?;
        let prev = self
            .track_volume_gesture_origin
            .remove(track_id)
            .unwrap_or(next);
        self.set_track_volume(track_id, next);
        if Self::fader_debug_enabled() {
            eprintln!("[fader] commit track={track_id} prev={prev:.4} next={next:.4}");
        }
        Some((prev, next))
    }

    pub fn clear_track_volume_preview(&mut self, track_id: &str) {
        self.track_volume_previews.remove(track_id);
        self.track_volume_gesture_origin.remove(track_id);
    }

    pub fn apply_volume_previews_to_snapshot(
        &self,
        tracks: &mut [TrackState],
        master: &mut MasterBusState,
    ) {
        if let Some(v) = self.master_volume_preview {
            master.volume = v;
        }
        for track in tracks {
            if let Some(v) = self.track_volume_previews.get(&track.id).copied() {
                track.volume = v;
                track.volume_effective = v;
            }
        }
    }

    /// Set a track's manual/base fader volume (the `UserFader` path). When
    /// automation read is off — or there is no active volume automation — the
    /// effective volume follows the base immediately so the display and runtime
    /// track the fader. When automation read is on with an active lane, base is
    /// updated underneath but effective stays automation-driven (DAW behavior).
    pub fn set_track_volume(&mut self, track_id: &str, norm: f32) {
        if self
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .is_some_and(|t| (t.volume - norm.clamp(0.0, 1.0)).abs() > 1.0e-5)
        {
            self.override_automation(track_id, is_volume);
        }
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            let v = norm.clamp(0.0, 1.0);
            t.volume = v;
            if !(t.volume_automation_read && t.has_active_volume_automation()) {
                t.volume_effective = v;
            }
            if automation_sync_debug_enabled() {
                eprintln!(
                    "[automation-sync] target=TrackVolume({}) base={:.3}({}) effective={:.3} reason=fader_drag",
                    t.id,
                    v,
                    volume::format_db(v),
                    t.volume_effective,
                );
            }
        }
    }

    /// Toggle whether Track Volume automation drives this track's effective
    /// value. Returns `true` if the flag changed. The caller should follow with
    /// [`Self::recompute_effective_volumes`] at the current playhead so the
    /// fader/inspector preview updates immediately.
    pub fn set_track_volume_automation_read(&mut self, track_id: &str, read: bool) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.volume_automation_read != read {
                t.volume_automation_read = read;
                if !read {
                    t.volume_effective = t.volume;
                }
                return true;
            }
        }
        false
    }

    /// Recompute every track's effective volume from its Track Volume automation
    /// lane at `beat`. UI-only: faders/inspector read [`TrackState::display_volume`]
    /// which prefers the effective value. Returns `true` if any effective value
    /// changed (so the caller can `notify`). `reason` is only used for the
    /// `[automation-sync]` trace and should be one of `playback_tick`, `seek`,
    /// or `point_edit`.
    pub fn recompute_effective_volumes(&mut self, beat: f32, reason: &str) -> bool {
        let debug = automation_sync_debug_enabled();
        let mut changed = false;
        for track in &mut self.tracks {
            // Without a curve to read, the effective volume is the base volume
            // and cannot have moved since the last tick. Checked before the
            // lane scan because this runs on the playback tick for every track,
            // and most projects automate the volume of none of them.
            if !track.volume_automation_read || track.automation_lanes.is_empty() {
                if (track.volume_effective - track.volume).abs() > 1.0e-5 {
                    track.volume_effective = track.volume;
                    changed = true;
                }
                continue;
            }
            let resolved = track
                .automation_lanes
                .iter()
                .find(|l| {
                    l.plays()
                        && matches!(l.target, AutomationTarget::TrackVolume)
                        && !l.points.is_empty()
                })
                .map(|l| evaluate_automation(&l.points, beat as f64, l.target.default_value()));
            let new_effective = match (track.volume_automation_read, resolved) {
                (true, Some(v)) => v,
                _ => track.volume,
            };
            if (track.volume_effective - new_effective).abs() > 1.0e-5 {
                if debug {
                    eprintln!(
                        "[automation-sync] target=TrackVolume({}) beat={:.3} value={:.3}({}) base={:.3}({}) effective {:.3}→{:.3} reason={}",
                        track.id,
                        beat,
                        new_effective,
                        volume::format_db(new_effective),
                        track.volume,
                        volume::format_db(track.volume),
                        track.volume_effective,
                        new_effective,
                        reason,
                    );
                }
                track.volume_effective = new_effective;
                changed = true;
            }
        }
        changed |= self.recompute_automated_sends(beat);
        changed
    }

    /// What each send's lanes play at `beat`, for the send controls to show
    /// (the engine plays the same lanes on its own). A lane shows only while
    /// it reads — enabled, with points, and not being recorded, when the
    /// control itself is what is heard. Returns whether any shown value moved.
    fn recompute_automated_sends(&mut self, beat: f32) -> bool {
        let mut changed = false;
        for track in &mut self.tracks {
            if track.sends.is_empty() {
                continue;
            }
            let lane_value = |want: &dyn Fn(&AutomationTarget) -> bool| {
                track
                    .automation_lanes
                    .iter()
                    .find(|l| l.plays() && !l.points.is_empty() && want(&l.target))
                    .map(|l| evaluate_automation(&l.points, beat as f64, l.target.default_value()))
            };
            let mut shown = Vec::with_capacity(track.sends.len());
            for send in &track.sends {
                let level = lane_value(
                    &|t| matches!(t, AutomationTarget::SendLevel { send_id } if *send_id == send.id),
                );
                let on = lane_value(
                    &|t| matches!(t, AutomationTarget::SendEnabled { send_id } if *send_id == send.id),
                );
                shown.push(match (on, level) {
                    (None, None) => None,
                    (on, level) => Some((
                        on.map_or(send.enabled, |v| v >= 0.5),
                        level.map_or(send.gain_db, volume::norm_to_db),
                    )),
                });
            }
            for (send, shown) in track.sends.iter_mut().zip(shown) {
                if send.automated != shown {
                    send.automated = shown;
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn set_track_pan(&mut self, track_id: &str, pan: f32) {
        if self
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .is_some_and(|t| (t.pan - pan.clamp(-1.0, 1.0)).abs() > 1.0e-5)
        {
            self.override_automation(track_id, is_pan);
        }
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            t.pan = pan.clamp(-1.0, 1.0);
        }
    }

    pub fn begin_track_pan_preview(&mut self, track_id: &str) {
        if self.track_pan_gesture_origin.contains_key(track_id) {
            return;
        }
        if let Some(pan) = self
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.pan)
        {
            self.track_pan_gesture_origin
                .insert(track_id.to_string(), pan);
        }
    }

    pub fn set_track_pan_preview(&mut self, track_id: &str, pan: f32) -> bool {
        let next = pan.clamp(-1.0, 1.0);
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        if (track.pan - next).abs() <= 1.0e-5 {
            return false;
        }
        track.pan = next;
        self.override_automation(track_id, is_pan);
        true
    }

    pub fn commit_track_pan_preview(&mut self, track_id: &str) -> Option<(f32, f32)> {
        let prev = self.track_pan_gesture_origin.remove(track_id)?;
        let next = self
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.pan)?;
        Some((prev, next))
    }

    pub fn toggle_track_mute(&mut self, track_id: &str) -> bool {
        self.override_automation(track_id, is_mute);
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            t.muted = !t.muted;
            return true;
        }
        false
    }

    /// Set mute without toggling. Returns `true` when the stored value changed.
    pub fn set_track_mute(&mut self, track_id: &str, muted: bool) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.muted != muted {
                t.muted = muted;
                self.override_automation(track_id, is_mute);
                return true;
            }
        }
        false
    }

    pub fn toggle_track_solo(&mut self, track_id: &str) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            t.solo = !t.solo;
            return true;
        }
        false
    }

    /// Set solo without toggling. Returns `true` when the stored value changed.
    pub fn set_track_solo(&mut self, track_id: &str, solo: bool) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.solo != solo {
                t.solo = solo;
                return true;
            }
        }
        false
    }

    /// Is anything currently muted / soloed anywhere in the arrangement?
    ///
    /// Backs the two global latches in the Arrangement header. They are
    /// indicators first: a mute left on a track scrolled off screen is the
    /// classic reason a mix has a hole in it, and until now nothing in the
    /// shell said so.
    pub fn any_track_muted(&self) -> bool {
        self.tracks.iter().any(|track| track.muted)
    }

    pub fn any_track_soloed(&self) -> bool {
        self.tracks.iter().any(|track| track.solo)
    }

    /// Unmute everything. Returns the ids that actually changed, so the caller
    /// pushes exactly those to the engine instead of re-sending the whole
    /// arrangement's mute state.
    pub fn clear_all_track_mutes(&mut self) -> Vec<String> {
        let mut cleared = Vec::new();
        for track in self.tracks.iter_mut() {
            if track.muted {
                track.muted = false;
                cleared.push(track.id.clone());
            }
        }
        cleared
    }

    /// Clear every solo. Same contract as [`Self::clear_all_track_mutes`].
    pub fn clear_all_track_solos(&mut self) -> Vec<String> {
        let mut cleared = Vec::new();
        for track in self.tracks.iter_mut() {
            if track.solo {
                track.solo = false;
                cleared.push(track.id.clone());
            }
        }
        cleared
    }

    pub fn toggle_track_arm(&mut self, track_id: &str) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            t.armed = !t.armed;
            return true;
        }
        false
    }

    pub fn cycle_track_input_monitor(&mut self, track_id: &str) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            t.input_monitor = t.input_monitor.cycle();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod control_room_state_tests {
    use super::*;

    /// The Control Room must come up monitoring the whole mix, never a
    /// capture device.
    #[test]
    fn default_monitor_source_is_the_master_bus() {
        let state = TimelineState::default();
        assert_eq!(state.monitor.source, MonitorSourceKind::MasterBus);
        assert_eq!(state.monitor.source_label(), "Master Bus");
        assert_eq!(
            state.monitor.output_name, "Out 1-2",
            "the chip carries the resolved destination label once refreshed"
        );
        assert!(!state.monitor.mute);
        assert!(!state.monitor.dim);
        assert!(!state.monitor.mono);
    }

    /// A hardware input is never offered as the *first* option, so cycling the
    /// Source chip can never land on a microphone before the master bus.
    #[test]
    fn source_options_start_at_master_and_offer_project_buses() {
        let mut state = TimelineState::default();
        let group = state.create_bus_track(&[]);
        let options = state.monitor_source_options();
        assert_eq!(options[0], MonitorSourceKind::MasterBus);
        assert!(
            options
                .iter()
                .any(|option| *option == MonitorSourceKind::Bus(group.clone())),
            "a project group bus should be selectable as a monitor source"
        );
        assert!(
            !options
                .iter()
                .any(|option| matches!(option, MonitorSourceKind::HardwareInput(_))),
            "hardware inputs must not appear in the default source cycle"
        );
    }

    /// The Source chip shows the bus's real name, not its internal id.
    #[test]
    fn selecting_a_bus_source_resolves_its_display_name() {
        let mut state = TimelineState::default();
        let group = state.create_bus_track(&[]);
        let name = state
            .find_track(&group)
            .map(|track| track.name.clone())
            .expect("group track");

        assert!(state.set_monitor_source(MonitorSourceKind::Bus(group.clone())));
        assert_eq!(state.monitor.source_label(), name);
        // Re-selecting the same source is a no-op.
        assert!(!state.set_monitor_source(MonitorSourceKind::Bus(group)));
    }

    #[test]
    fn monitor_toggles_are_independent_and_do_not_touch_the_master_bus() {
        let mut state = TimelineState::default();
        let master_before = state.master.clone();

        state.toggle_monitor_mute();
        state.toggle_monitor_dim();
        state.toggle_monitor_mono();
        assert!(state.monitor.mute);
        assert!(state.monitor.dim);
        assert!(state.monitor.mono);

        state.toggle_monitor_dim();
        assert!(state.monitor.mute, "dim must not clear mute");
        assert!(!state.monitor.dim);

        assert_eq!(
            state.master, master_before,
            "Control Room controls must never mutate master bus state"
        );
    }

    /// The Monitor Output chip reads a logical connection, and Follow Master
    /// Output is what it shows before any destination is assigned.
    #[test]
    fn the_monitor_output_chip_reads_the_effective_connection() {
        use crate::audio_connections::{
            AudioConnection, AudioConnectionDirection, AvailablePorts, ChannelLayout,
        };

        let ports = AvailablePorts::for_device("dev-1", "Interface", 2, 4);
        let mut state = TimelineState::default();
        state.refresh_output_labels();
        assert_eq!(
            state.monitor.output_name,
            "Follow Master Output — No Master Output"
        );

        let main = state.audio_connections.add(
            AudioConnection::new(
                "Main Output",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 0, |i| format!("Output {}", i + 1)),
        );
        let headphones = state.audio_connections.add(
            AudioConnection::new(
                "Headphones",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 2, |i| format!("Output {}", i + 1)),
        );
        state.audio_connections.revalidate(&ports);

        state.set_master_output_connection(Some(main));
        state.refresh_output_labels();
        assert_eq!(state.master.output_label, "Main Output");
        assert_eq!(state.monitor.output_name, "Follow Master Output");

        state.set_monitor_output_connection(Some(headphones));
        state.refresh_output_labels();
        assert_eq!(state.monitor.output_name, "Headphones");
        assert_eq!(
            state.master.output_label, "Main Output",
            "the Monitor override must not change the Master destination"
        );
    }

    #[test]
    fn monitor_volume_reports_only_real_changes() {
        let mut state = TimelineState::default();
        let start = state.monitor.volume;
        assert!(!state.set_monitor_volume(start));
        assert!(state.set_monitor_volume(0.25));
        assert!((state.monitor.volume - 0.25).abs() < 1.0e-6);
    }
}

#[cfg(test)]
mod listen_state_tests {
    use super::*;

    fn two_tracks() -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        let a = state.create_audio_track();
        let b = state.create_audio_track();
        (state, a, b)
    }

    #[test]
    fn tracks_start_with_listen_off() {
        let (state, a, b) = two_tracks();
        assert_eq!(state.find_track(&a).unwrap().listen, ListenMode::Off);
        assert_eq!(state.find_track(&b).unwrap().listen, ListenMode::Off);
        assert!(state.active_listen_track().is_none());
        assert!(!state.monitor.listen_active);
    }

    /// Listen is exclusive: the Control Room must always be on one
    /// unambiguous tap, never a sum of channels the user did not compare.
    #[test]
    fn engaging_listen_clears_it_on_every_other_channel() {
        let (mut state, a, b) = two_tracks();

        assert!(state.set_track_listen(&a, ListenMode::Pfl));
        assert_eq!(state.find_track(&a).unwrap().listen, ListenMode::Pfl);
        assert_eq!(state.find_track(&b).unwrap().listen, ListenMode::Off);
        assert_eq!(
            state.active_listen_track(),
            Some((a.as_str(), ListenMode::Pfl))
        );
        assert!(state.monitor.listen_active);

        assert!(state.set_track_listen(&b, ListenMode::Afl));
        assert_eq!(state.find_track(&a).unwrap().listen, ListenMode::Off);
        assert_eq!(state.find_track(&b).unwrap().listen, ListenMode::Afl);
        assert_eq!(
            state.active_listen_track(),
            Some((b.as_str(), ListenMode::Afl))
        );
    }

    /// PFL and AFL on the same channel replace each other rather than stacking.
    #[test]
    fn switching_between_pfl_and_afl_on_one_channel_replaces_the_mode() {
        let (mut state, a, _) = two_tracks();
        assert!(state.set_track_listen(&a, ListenMode::Pfl));
        assert!(state.set_track_listen(&a, ListenMode::Afl));
        assert_eq!(state.find_track(&a).unwrap().listen, ListenMode::Afl);
        assert!(state.monitor.listen_active);
    }

    /// Clicking the engaged mode again clears it and hands monitoring back to
    /// the Control Room's selected source.
    #[test]
    fn re_clicking_the_engaged_mode_returns_to_the_selected_source() {
        let (mut state, a, _) = two_tracks();
        state.set_track_listen(&a, ListenMode::Pfl);

        assert!(state.set_track_listen(&a, ListenMode::Pfl));
        assert_eq!(state.find_track(&a).unwrap().listen, ListenMode::Off);
        assert!(state.active_listen_track().is_none());
        assert!(
            !state.monitor.listen_active,
            "with no Listen engaged the Control Room falls back to its source"
        );
    }

    #[test]
    fn clear_all_listen_resets_every_channel_and_reports_change() {
        let (mut state, a, _) = two_tracks();
        state.set_track_listen(&a, ListenMode::Afl);

        assert!(state.clear_all_listen());
        assert!(state.active_listen_track().is_none());
        assert!(!state.monitor.listen_active);
        // Idempotent — nothing left to clear.
        assert!(!state.clear_all_listen());
    }

    /// Listen must never alter what the channel sends to the master mix,
    /// otherwise it could change an export or a recording.
    #[test]
    fn listen_does_not_touch_mix_affecting_track_state() {
        let (mut state, a, _) = two_tracks();
        let before = state.find_track(&a).cloned().unwrap();

        state.set_track_listen(&a, ListenMode::Pfl);
        let after = state.find_track(&a).cloned().unwrap();

        assert_eq!(after.volume, before.volume);
        assert_eq!(after.pan, before.pan);
        assert_eq!(after.muted, before.muted);
        assert_eq!(after.solo, before.solo);
        assert_eq!(after.routing, before.routing);
        assert_eq!(after.sends, before.sends);
        assert_ne!(after.listen, before.listen);
    }

    #[test]
    fn listen_mode_maps_to_the_engine_equivalent() {
        assert_eq!(
            ListenMode::Off.to_engine(),
            DirectAudio::monitor::ListenMode::Off
        );
        assert_eq!(
            ListenMode::Pfl.to_engine(),
            DirectAudio::monitor::ListenMode::Pfl
        );
        assert_eq!(
            ListenMode::Afl.to_engine(),
            DirectAudio::monitor::ListenMode::Afl
        );
    }
}

/// Lane targets a manual move of each control overrides.
fn is_volume(target: &AutomationTarget) -> bool {
    matches!(target, AutomationTarget::TrackVolume)
}

fn is_pan(target: &AutomationTarget) -> bool {
    matches!(target, AutomationTarget::TrackPan)
}

fn is_mute(target: &AutomationTarget) -> bool {
    matches!(target, AutomationTarget::TrackMute)
}

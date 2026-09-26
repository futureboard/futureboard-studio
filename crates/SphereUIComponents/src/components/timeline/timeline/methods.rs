//! Split out of `timeline.rs` (god-file decomposition): inherent `impl Timeline`.

use super::*;

impl Timeline {
    pub(super) fn hit_test_debug_enabled() -> bool {
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_HITTEST_DEBUG").is_some())
    }

    pub(super) fn input_debug_enabled() -> bool {
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| {
            std::env::var_os("FUTUREBOARD_TIMELINE_INPUT_DEBUG").is_some()
                || std::env::var_os("FUTUREBOARD_SELECTION_DEBUG").is_some()
        })
    }

    pub(super) fn log_input_state(&self, label: &str) {
        if Self::input_debug_enabled() {
            eprintln!(
                "[timeline-input] {label} pen_drag={} range_drag={} erase_drag={} automation_drag={} automation_marquee={} tempo_drag={} clip_drag_origin={} pan_drag={}",
                self.pen_clip_draw.is_some(),
                self.range_select_drag.is_some(),
                self.erase_clip_drag.is_some(),
                self.automation_drag.is_some(),
                self.automation_marquee.is_some(),
                self.tempo_drag.is_some(),
                self.clip_drag_origin.is_some(),
                self.pan_last_position.is_some(),
            );
        }
    }

    pub(super) fn arrangement_coordinate_context(&self) -> ArrangementCoordinateContext {
        // Both origins come off the one measured lane edge. Deriving the panel
        // origin from the chrome estimate while the viewport used the measured
        // value would let `panel_x` and `viewport_x` disagree about where the
        // track headers end.
        let lane_origin_x = self.state.lane_origin_x();
        let origin_y = self.state.timeline_origin_y();
        let panel_origin_px = gpui::point(px(lane_origin_x - HEADER_WIDTH), px(origin_y));
        let viewport_origin_px = gpui::point(
            px(lane_origin_x),
            px(origin_y + self.state.arrangement_content_top()),
        );
        ArrangementCoordinateContext {
            panel_origin_px,
            viewport_origin_px,
            scroll_x_px: self.state.viewport.scroll_x,
            scroll_y_px: self.state.viewport.scroll_y,
            zoom_px_per_beat: self.state.viewport.pixels_per_beat.max(0.0001),
            ruler_height_px: RULER_HEIGHT,
            track_header_width_px: HEADER_WIDTH,
        }
    }

    pub(crate) fn resolve_context_target_from_window_point(
        &self,
        position: gpui::Point<gpui::Pixels>,
    ) -> TimelineContextTarget {
        let ctx = self.arrangement_coordinate_context();
        let result = hit_test_arrangement(&self.state, position, &ctx);
        if Self::hit_test_debug_enabled() {
            let screen_x: f32 = position.x.into();
            let screen_y: f32 = position.y.into();
            eprintln!(
                "Arrangement hit-test:\nscreen=({screen_x:.1},{screen_y:.1})\nlocal=({:.1},{:.1})\ntarget={}\n{}\nz_priority={}",
                result.local.viewport_x,
                result.local.viewport_y,
                result.target.kind(),
                format_arrangement_target_debug(&result.target),
                result.z_priority,
            );
        }
        match result.target {
            ArrangementHitTarget::EmptyArrangement { .. } => TimelineContextTarget::TimelineEmpty,
            ArrangementHitTarget::TrackHeader { track_id } => {
                TimelineContextTarget::TrackHeader(track_id)
            }
            ArrangementHitTarget::TrackLane {
                track_id,
                timeline_beat,
            } => TimelineContextTarget::TrackLane {
                track_id,
                beat: timeline_beat,
            },
            ArrangementHitTarget::AudioClip {
                track_id,
                clip_id,
                timeline_beat,
                local_beat,
            } => TimelineContextTarget::AudioClip {
                track_id,
                clip_id,
                beat: timeline_beat,
                local_beat,
            },
            ArrangementHitTarget::MidiClip {
                track_id,
                clip_id,
                timeline_beat,
                local_beat,
            } => TimelineContextTarget::MidiClip {
                track_id,
                clip_id,
                beat: timeline_beat,
                local_beat,
            },
            ArrangementHitTarget::VideoClip { clip_id, .. } => TimelineContextTarget::Clip(clip_id),
            ArrangementHitTarget::Ruler { timeline_beat } => {
                TimelineContextTarget::Ruler(timeline_beat)
            }
            ArrangementHitTarget::Marker {
                marker_id,
                timeline_beat,
            } => TimelineContextTarget::Marker {
                marker_id,
                beat: timeline_beat,
            },
            ArrangementHitTarget::AutomationLane {
                track_id,
                lane_id,
                timeline_beat,
            } => TimelineContextTarget::AutomationLane {
                track_id,
                lane_id,
                beat: timeline_beat,
            },
        }
    }

    pub fn reset_input_state(&mut self) {
        self.log_input_state("reset-before");
        self.file_drop_hint = None;
        self.clip_clone_hint = None;
        self.song_text_drag_preview = None;
        self.clip_drag_origin = None;
        self.clip_resize_origin = None;
        // The overlay renders with this view, so clearing the cell is enough
        // for the caller's notify to take the razor line down.
        self.cut_guide.set(None);
        self.clip_drag_target_track_index = None;
        self.clip_clone_drag_id = None;
        self.pen_clip_draw = None;
        // Ends a marquee and keeps the selection it previewed; Escape goes
        // through `cancel_marquee` first to put the previous one back. The
        // rectangle's overlay, like the razor line's, renders with this view.
        self.end_marquee(true);
        self.marquee_autoscroll = None;
        self.marquee_frame.set(None);
        self.state.arrangement_range = None;
        // A clip move that ends here was never committed — Escape, a tool
        // change, a cancelled gesture — so its clips go back where the gesture
        // found them rather than stay moved with no undo step.
        self.revert_clip_move();
        self.pending_clip_click = None;
        self.cut_hover = None;
        self.erase_clip_drag = None;
        self.erase_preview_ids.clear();
        self.automation_drag = None;
        self.automation_curve_drag = None;
        self.automation_marquee = None;
        self.tempo_drag = None;
        self.tempo_gesture_origin = None;
        self.tempo_gesture_linear_anchors.clear();
        self.ts_drag = None;
        self.ts_gesture_origin = None;
        self.marker_drag = None;
        self.region_gesture_origin = None;
        self.chord_gesture_origin = None;
        self.marker_gesture_origin = None;
        self.pan_last_position = None;
        self.state.clear_track_drag();
        self.state.cancel_track_height_resize();
        self.state.cancel_global_lane_resize();
        self.log_input_state("reset-after");
    }

    pub(super) fn cancel_active_gesture(&mut self, cx: &mut gpui::Context<Self>) {
        if Self::input_debug_enabled() {
            eprintln!("[selection] marquee_cancel");
        }
        // Restores before the blanket reset drops the snapshot it needs.
        self.cancel_marquee(cx);
        self.cancel_marker_track_interaction(cx);
        self.reset_input_state();
        self.song_text_drag_cancelled = true;
        cx.notify();
    }

    /// Clean empty-project Timeline — the real runtime entry point.
    pub fn new() -> Self {
        Self {
            state: TimelineState::default(),
            edit_history: EditHistory::new(100),
            inspector_clip_gesture_origin: None,
            on_seek_beats: None,
            on_track_param_change: None,
            on_track_input_state_change: None,
            on_project_changed: None,
            on_midi_changed: None,
            on_control_state_changed: None,
            on_loop_changed: None,
            on_tempo_map_changed: None,
            on_time_signature_map_changed: None,
            on_musical_context_changed: None,
            on_media_changed: None,
            on_add_track: None,
            on_plugin_preset_drop: None,
            on_plugin_drag_drop: None,
            plugin_drop_hint: None,
            on_midi_import_prompt: None,
            last_drag_position: None,
            file_drop_hint: None,
            clip_clone_hint: None,
            song_text_drag_preview: None,
            song_text_drag_cancelled: false,
            clip_drag_origin: None,
            clip_resize_origin: None,
            clip_resize_grab_beats: 0.0,
            clip_drag_target_track_index: None,
            clip_clone_drag_id: None,
            pen_clip_draw: None,
            range_select_drag: None,
            erase_clip_drag: None,
            erase_preview_ids: HashSet::new(),
            automation_drag: None,
            automation_curve_drag: None,
            automation_marquee: None,
            automation_hover: None,
            on_automation_control: None,
            tempo_drag: None,
            marker_drag: None,
            tempo_gesture_origin: None,
            tempo_gesture_linear_anchors: Vec::new(),
            ts_drag: None,
            ts_gesture_origin: None,
            region_gesture_origin: None,
            chord_gesture_origin: None,
            on_command: None,
            marker_gesture_origin: None,
            pan_last_position: None,
            track_zoom_session: None,
            floating_toolbar_position: None,
            floating_toolbar_drag_anchor: None,
            on_context_menu: None,
            on_playhead_scrub_begin: None,
            on_playhead_scrub_end: None,
            on_open_editor: None,
            on_tempo_point_edit: None,
            on_open_song_text_editor: None,
            on_open_track_instrument: None,
            chrome_metrics: TimelineChromeMetrics::default(),
            lane_origin_probe: std::rc::Rc::new(std::cell::Cell::new(None)),
            ruler_gesture: std::rc::Rc::new(std::cell::Cell::new(Default::default())),
            playhead_frame: std::rc::Rc::new(std::cell::Cell::new(
                crate::components::timeline::playhead::PlayheadFrame::default(),
            )),
            playhead_overlay: None,
            track_meters: Default::default(),
            arrangement_surface: None,
            track_lanes: Default::default(),
            frame_lane_ctx: None,
            project_root: None,
            focus_lost_subscription: None,
            clip_process_origin: None,
            snap_menu_open: false,
            cut_guide: Default::default(),
            cut_guide_overlay: None,
            cut_hover: None,
            timeline_origin_probe: std::rc::Rc::new(std::cell::Cell::new(None)),
            marquee_frame: Default::default(),
            marquee_overlay: None,
            marquee_autoscroll: None,
            clip_move_origin: None,
            clip_drag_cancelled: false,
            pending_clip_click: None,
            track_rename: None,
            focus_return: None,
            clip_fades: Default::default(),
        }
    }

    /// Seeded demo Timeline. Use only from explicit dev/demo entry points;
    /// production startup should always call [`Timeline::new`].
    pub fn with_demo_content() -> Self {
        Self {
            state: TimelineState::demo_project(),
            edit_history: EditHistory::new(100),
            inspector_clip_gesture_origin: None,
            on_seek_beats: None,
            on_track_param_change: None,
            on_track_input_state_change: None,
            on_project_changed: None,
            on_midi_changed: None,
            on_control_state_changed: None,
            on_loop_changed: None,
            on_tempo_map_changed: None,
            on_time_signature_map_changed: None,
            on_musical_context_changed: None,
            on_media_changed: None,
            on_add_track: None,
            on_plugin_preset_drop: None,
            on_plugin_drag_drop: None,
            plugin_drop_hint: None,
            on_midi_import_prompt: None,
            last_drag_position: None,
            file_drop_hint: None,
            clip_clone_hint: None,
            song_text_drag_preview: None,
            song_text_drag_cancelled: false,
            clip_drag_origin: None,
            clip_resize_origin: None,
            clip_resize_grab_beats: 0.0,
            clip_drag_target_track_index: None,
            clip_clone_drag_id: None,
            pen_clip_draw: None,
            range_select_drag: None,
            erase_clip_drag: None,
            erase_preview_ids: HashSet::new(),
            automation_drag: None,
            automation_curve_drag: None,
            automation_marquee: None,
            automation_hover: None,
            on_automation_control: None,
            tempo_drag: None,
            marker_drag: None,
            tempo_gesture_origin: None,
            tempo_gesture_linear_anchors: Vec::new(),
            ts_drag: None,
            ts_gesture_origin: None,
            region_gesture_origin: None,
            chord_gesture_origin: None,
            on_command: None,
            marker_gesture_origin: None,
            pan_last_position: None,
            track_zoom_session: None,
            floating_toolbar_position: None,
            floating_toolbar_drag_anchor: None,
            on_context_menu: None,
            on_playhead_scrub_begin: None,
            on_playhead_scrub_end: None,
            on_open_editor: None,
            on_tempo_point_edit: None,
            on_open_song_text_editor: None,
            on_open_track_instrument: None,
            chrome_metrics: TimelineChromeMetrics::default(),
            lane_origin_probe: std::rc::Rc::new(std::cell::Cell::new(None)),
            ruler_gesture: std::rc::Rc::new(std::cell::Cell::new(Default::default())),
            playhead_frame: std::rc::Rc::new(std::cell::Cell::new(
                crate::components::timeline::playhead::PlayheadFrame::default(),
            )),
            playhead_overlay: None,
            track_meters: Default::default(),
            arrangement_surface: None,
            track_lanes: Default::default(),
            frame_lane_ctx: None,
            project_root: None,
            focus_lost_subscription: None,
            clip_process_origin: None,
            snap_menu_open: false,
            cut_guide: Default::default(),
            cut_guide_overlay: None,
            cut_hover: None,
            timeline_origin_probe: std::rc::Rc::new(std::cell::Cell::new(None)),
            marquee_frame: Default::default(),
            marquee_overlay: None,
            marquee_autoscroll: None,
            clip_move_origin: None,
            clip_drag_cancelled: false,
            pending_clip_click: None,
            track_rename: None,
            focus_return: None,
            clip_fades: Default::default(),
        }
    }

    pub fn run_edit_command(&mut self, cmd: EditCommand, cx: &mut gpui::Context<Self>) {
        self.clip_process_origin = None;
        let impact = cmd.impact();
        cmd.execute(&mut self.state);
        self.edit_history.push(cmd);
        self.notify_edit_impact(impact, cx);
        cx.notify();
    }

    /// Propagate one command's effect. Execute, record, undo, and redo all go
    /// through here so an undone tempo/meter edit reaches the engine exactly
    /// like the original edit did.
    fn notify_edit_impact(&self, impact: EditImpact, cx: &mut gpui::App) {
        match impact {
            EditImpact::Project => self.mark_project_changed(cx),
            EditImpact::Midi => self.mark_midi_changed(cx),
            EditImpact::Metadata => self.mark_control_state_changed(cx),
            EditImpact::MixerControl => self.mark_control_state_changed(cx),
            EditImpact::TempoMap => self.mark_tempo_map_changed(cx),
            EditImpact::TimeSignatureMap => self.mark_time_signature_map_changed(cx),
            EditImpact::MusicalContext => self.mark_musical_context_changed(cx),
        }
    }

    /// Execute a persisted UI-metadata edit without invalidating the audio graph.
    pub fn run_metadata_edit_command(&mut self, cmd: EditCommand, cx: &mut gpui::Context<Self>) {
        debug_assert!(cmd.is_metadata_only());
        cmd.execute(&mut self.state);
        self.edit_history.push(cmd);
        self.mark_control_state_changed(cx);
        cx.notify();
    }

    /// Record an automation edit that has already been applied, given the lane
    /// snapshot taken before it.
    ///
    /// No-ops when the lanes are unchanged, so a gesture that ended up doing
    /// nothing (a click that selected but moved nothing, a clear on an empty
    /// lane) never buries a real edit under a dead undo step.
    pub fn record_automation_lanes_edit(
        &mut self,
        track_id: &str,
        prev: Vec<crate::components::timeline::timeline_state::AutomationLaneState>,
        cx: &mut gpui::Context<Self>,
    ) {
        let next = self.state.capture_automation_lanes(track_id);
        if next == prev {
            return;
        }
        self.record_executed_command(
            EditCommand::SetTrackAutomationLanes {
                track_id: track_id.to_string(),
                prev,
                next,
            },
            cx,
        );
    }

    /// Record a command whose effect has already been applied to the state
    /// (e.g. a gesture that mutated `state` live). Pushes it onto the undo
    /// stack without re-executing, then marks the project changed.
    pub fn record_executed_command(&mut self, cmd: EditCommand, cx: &mut gpui::Context<Self>) {
        // A clip gain/fade gesture whose release never arrived must not lend
        // its stale "before" to the next one.
        self.clip_process_origin = None;
        let impact = cmd.impact();
        self.edit_history.push(cmd);
        self.notify_edit_impact(impact, cx);
        cx.notify();
    }

    /// Record a Record pass that has already been applied (see
    /// [`EditCommand::record_pass`]), merging it with the same pass's earlier
    /// half so one take is one undo step.
    pub fn record_pass_command(&mut self, cmd: EditCommand, cx: &mut gpui::Context<Self>) {
        self.clip_process_origin = None;
        let impact = cmd.impact();
        self.edit_history.push_record_pass(cmd);
        self.notify_edit_impact(impact, cx);
        cx.notify();
    }

    /// Capture the tempo state before a Tempo-lane mutation. Pair with
    /// [`Self::record_tempo_edit`].
    pub(crate) fn capture_tempo_state(&self) -> TempoStateSnapshot {
        TempoStateSnapshot::capture(&self.state)
    }

    /// Record an already-applied Tempo-lane edit as one undo entry. No-ops (and
    /// returns `false`) when the gesture left the tempo state unchanged, so a
    /// click that moved nothing never buries a real edit under a dead step.
    pub(crate) fn record_tempo_edit(
        &mut self,
        label: &'static str,
        prev: TempoStateSnapshot,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let next = TempoStateSnapshot::capture(&self.state);
        if next == prev {
            return false;
        }
        self.record_executed_command(EditCommand::SetTempoState { label, prev, next }, cx);
        true
    }

    /// Fold another step of a repeated tempo gesture into the entry it already
    /// pushed. Falls back to a normal recorded edit when there is nothing to
    /// extend, so the gesture is always undoable either way.
    pub(crate) fn amend_or_record_tempo_edit(
        &mut self,
        label: &'static str,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let next = TempoStateSnapshot::capture(&self.state);
        if !self.edit_history.amend_tempo_state(label, next) {
            return false;
        }
        self.mark_tempo_map_changed(cx);
        cx.notify();
        true
    }

    pub(crate) fn capture_time_signature_state(&self) -> TimeSignatureStateSnapshot {
        TimeSignatureStateSnapshot::capture(&self.state)
    }

    /// Record an already-applied Time Signature-lane edit as one undo entry.
    pub(crate) fn record_time_signature_edit(
        &mut self,
        label: &'static str,
        prev: TimeSignatureStateSnapshot,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let next = TimeSignatureStateSnapshot::capture(&self.state);
        if next == prev {
            return false;
        }
        self.record_executed_command(EditCommand::SetTimeSignatureState { label, prev, next }, cx);
        true
    }

    /// Record an already-applied arrangement-marker edit as one undo entry.
    pub(crate) fn record_marker_edit(
        &mut self,
        label: &'static str,
        prev: Vec<crate::components::timeline::timeline_state::TimelineMarkerState>,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        if self.state.markers == prev {
            return false;
        }
        let next = self.state.markers.clone();
        self.record_executed_command(EditCommand::SetMarkers { label, prev, next }, cx);
        true
    }

    /// Record an already-applied arrangement-region edit as one undo entry.
    pub(crate) fn record_region_edit(
        &mut self,
        label: &'static str,
        prev: Vec<TimelineRegionState>,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        if self.state.regions == prev {
            return false;
        }
        let next = self.state.regions.clone();
        self.record_executed_command(EditCommand::SetRegions { label, prev, next }, cx);
        true
    }

    /// One undo entry for a Chord Track change; a no-op when nothing changed.
    pub(crate) fn record_chord_edit(
        &mut self,
        label: &'static str,
        prev: Vec<crate::components::timeline::timeline_state::ChordTrackEvent>,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        if self.state.chord_events == prev {
            return false;
        }
        let next = self.state.chord_events.clone();
        self.record_executed_command(EditCommand::SetChordEvents { label, prev, next }, cx);
        true
    }

    pub fn begin_inspector_clip_gesture(&mut self, clip_id: &str) {
        self.inspector_clip_gesture_origin = ClipSnapshot::capture(&self.state, clip_id);
    }

    pub fn commit_inspector_clip_gesture(
        &mut self,
        clip_id: &str,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let Some(previous) = self.inspector_clip_gesture_origin.take() else {
            return false;
        };
        if previous.clip.id != clip_id {
            return false;
        }
        let Some(next) = ClipSnapshot::capture(&self.state, clip_id) else {
            return false;
        };
        // The transient `dirty` flag alone is no edit.
        let unchanged = crate::components::timeline::timeline_state::clip_edit_is_noop(
            &previous.clip,
            &next.clip,
        );
        if unchanged && previous.track_id == next.track_id {
            return false;
        }
        self.record_executed_command(EditCommand::UpdateClip { previous, next }, cx);
        true
    }

    /// Roll back a live Inspector/Audio Editor preview without creating an
    /// undo entry. Pointer edits are applied to the project model while the
    /// gesture is in flight so the canvas can render the preview; Escape,
    /// selection changes, and focus loss must therefore restore the exact
    /// pre-gesture clip rather than merely dropping the drag flag.
    pub fn cancel_inspector_clip_gesture(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        let Some(previous) = self.inspector_clip_gesture_origin.take() else {
            return false;
        };

        let mut restored = false;
        for track in &mut self.state.tracks {
            if let Some(clip) = track
                .clips
                .iter_mut()
                .find(|clip| clip.id == previous.clip.id)
            {
                *clip = previous.clip.clone();
                restored = true;
                break;
            }
        }
        if !restored {
            // A clip should not disappear during an Audio Editor gesture, but
            // restoring the snapshot is safer than leaving a half-applied
            // preview behind if another command removed it concurrently.
            if let Some(track) = self
                .state
                .tracks
                .iter_mut()
                .find(|track| track.id == previous.track_id)
            {
                track.clips.push(previous.clip);
                restored = true;
            }
        }
        if restored {
            cx.notify();
        }
        restored
    }

    /// Push the value a just-applied [`EditImpact::MixerControl`] command left in
    /// `state` straight down the realtime control path.
    ///
    /// Fader/pan edits no longer dirty the engine graph, so nothing else would
    /// carry an undo/redo of one to the audio thread — the UI would show the
    /// restored fader while the engine kept playing at the other value.
    fn push_mixer_control_to_engine(&self, cmd: Option<&EditCommand>) {
        let Some((track_id, param_id)) = cmd.and_then(EditCommand::mixer_control_target) else {
            return;
        };
        let Some(callback) = self.on_track_param_change.as_ref() else {
            return;
        };
        let Some(track) = self.state.tracks.iter().find(|t| t.id == track_id) else {
            return;
        };
        let value = match param_id {
            "volume" => track.display_volume(),
            "pan" => track.pan,
            _ => return,
        };
        callback(track_id.to_string(), param_id.to_string(), value);
    }

    pub fn undo_edit(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        if let Some(impact) = self.edit_history.undo_with_impact(&mut self.state) {
            if impact == EditImpact::MixerControl {
                self.push_mixer_control_to_engine(self.edit_history.last_undone());
            }
            self.notify_edit_impact(impact, cx);
            cx.notify();
            true
        } else {
            false
        }
    }

    pub fn redo_edit(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        if let Some(impact) = self.edit_history.redo_with_impact(&mut self.state) {
            if impact == EditImpact::MixerControl {
                self.push_mixer_control_to_engine(self.edit_history.last_redone());
            }
            self.notify_edit_impact(impact, cx);
            cx.notify();
            true
        } else {
            false
        }
    }

    /// The command the last [`Self::undo_edit`] reverted, for owners that
    /// must follow some kinds of undo up with work of their own.
    pub fn last_undone_edit(&self) -> Option<&EditCommand> {
        self.edit_history.last_undone()
    }

    /// The command the last [`Self::redo_edit`] re-applied.
    pub fn last_redone_edit(&self) -> Option<&EditCommand> {
        self.edit_history.last_redone()
    }

    pub fn delete_clip_command(&mut self, clip_id: &str, cx: &mut gpui::Context<Self>) {
        let Some(snapshot) = ClipSnapshot::capture(&self.state, clip_id) else {
            return;
        };
        self.run_edit_command(EditCommand::DeleteClip { snapshot }, cx);
    }

    /// Split an audio clip into two abutting clips at `split_beat` (absolute
    /// timeline beats). Returns `true` when a split was recorded. Shared by the
    /// Split-at-Playhead command and the Cut/razor tool click, so both routes
    /// produce one identical, undoable `ReplaceClipWithClips`.
    ///
    /// A no-op when the clip is missing, is not audio, or `split_beat` lands
    /// within `MIN_SPLIT_LEN_BEATS` of either edge (a hair-thin fragment is
    /// never useful and would round to a zero-length clip).
    pub fn split_audio_clip_at_beat(
        &mut self,
        clip_id: &str,
        split_beat: f32,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let Some(snapshot) = ClipSnapshot::capture(&self.state, clip_id) else {
            return false;
        };
        let Some((left, right)) = self.state.plan_audio_clip_split(&snapshot.clip, split_beat)
        else {
            return false;
        };

        let track_id = snapshot.track_id.clone();
        self.run_edit_command(
            EditCommand::ReplaceClipWithClips {
                clips: vec![(track_id.clone(), left), (track_id, right)],
                snapshot,
            },
            cx,
        );
        true
    }

    pub(super) fn beat_from_window_x(&self, x: f32) -> f32 {
        self.state.beats_from_window_x(x)
    }

    pub(super) fn snap_beat(&self, beat: f32) -> f32 {
        self.snap_beat_with_bypass(beat, false)
    }

    pub(super) fn snap_beat_with_bypass(&self, beat: f32, bypass: bool) -> f32 {
        self.state.snap_beats_with_bypass(beat, bypass)
    }

    /// Push the measured chrome panel sizes that surround the timeline so
    /// `scroll_geometry` can compute the real available body rect. Called
    /// by `StudioLayout` each render — cheap, no notify.
    pub fn set_chrome_metrics(&mut self, metrics: TimelineChromeMetrics) {
        self.chrome_metrics = metrics;
        // The browser panel is collapsible, so the timeline's own window-space
        // origin moves with it. Publishing it into the viewport keeps pointer
        // gestures (clip move/resize, ruler, lane tools) on the same transform
        // used to draw, instead of assuming the panel is always open.
        self.state.viewport.panel_origin_x = metrics.browser_width;
    }

    /// Push the current project's root folder (or `None` when Untitled). Called
    /// by `StudioLayout` each render — cheap, no notify. Drives eager
    /// copy-into-project for dropped audio.
    pub fn set_project_root(&mut self, root: Option<std::path::PathBuf>) {
        self.project_root = root;
    }

    pub fn set_command_callback(&mut self, callback: Option<TimelineCommandCb>) {
        self.on_command = callback;
    }

    /// Ask the Studio to run a named command. Deferred by the owner, so it is
    /// safe from inside a timeline listener.
    pub(crate) fn request_command(&self, command: &'static str, cx: &mut gpui::App) {
        if let Some(cb) = self.on_command.as_ref() {
            cb(command, cx);
        }
    }

    pub fn set_context_menu_callback(&mut self, callback: Option<TimelineContextMenuCb>) {
        self.on_context_menu = callback;
    }

    pub fn set_automation_control_callback(
        &mut self,
        callback: Option<
            crate::components::timeline::automation_control_lane::AutomationControlCallback,
        >,
    ) {
        self.on_automation_control = callback;
    }

    pub fn set_open_editor_callback(&mut self, callback: Option<TimelineOpenEditorCb>) {
        self.on_open_editor = callback;
    }

    pub fn set_tempo_point_edit_callback(&mut self, callback: Option<TempoPointEditCb>) {
        self.on_tempo_point_edit = callback;
    }

    pub fn set_open_song_text_editor_callback(&mut self, callback: Option<TimelineOpenEditorCb>) {
        self.on_open_song_text_editor = callback;
    }

    pub fn set_open_track_instrument_callback(
        &mut self,
        callback: Option<TimelineOpenTrackInstrumentCb>,
    ) {
        self.on_open_track_instrument = callback;
    }

    pub fn set_add_track_callback(&mut self, callback: Option<TimelineAddTrackCb>) {
        self.on_add_track = callback;
    }

    pub fn set_plugin_preset_drop_callback(
        &mut self,
        callback: Option<TimelinePluginPresetDropCb>,
    ) {
        self.on_plugin_preset_drop = callback;
    }

    pub fn set_plugin_drag_drop_callback(&mut self, callback: Option<TimelinePluginDragDropCb>) {
        self.on_plugin_drag_drop = callback;
    }

    pub fn set_midi_import_prompt_callback(
        &mut self,
        callback: Option<TimelineMidiImportPromptCb>,
    ) {
        self.on_midi_import_prompt = callback;
    }

    pub fn set_project_changed_callback(&mut self, callback: Option<TimelineProjectChangedCb>) {
        self.on_project_changed = callback;
    }

    pub fn set_midi_changed_callback(&mut self, callback: Option<TimelineProjectChangedCb>) {
        self.on_midi_changed = callback;
    }

    pub fn set_control_state_changed_callback(
        &mut self,
        callback: Option<TimelineProjectChangedCb>,
    ) {
        self.on_control_state_changed = callback;
    }

    pub fn set_loop_changed_callback(&mut self, callback: Option<TimelineProjectChangedCb>) {
        self.on_loop_changed = callback;
    }

    pub fn set_tempo_map_changed_callback(&mut self, callback: Option<TimelineProjectChangedCb>) {
        self.on_tempo_map_changed = callback;
    }

    pub fn set_time_signature_map_changed_callback(
        &mut self,
        callback: Option<TimelineProjectChangedCb>,
    ) {
        self.on_time_signature_map_changed = callback;
    }

    pub fn set_musical_context_changed_callback(
        &mut self,
        callback: Option<TimelineProjectChangedCb>,
    ) {
        self.on_musical_context_changed = callback;
    }

    pub fn set_media_changed_callback(&mut self, callback: Option<TimelineProjectChangedCb>) {
        self.on_media_changed = callback;
    }

    pub(crate) fn mark_tempo_map_changed(&self, cx: &mut gpui::App) {
        if let Some(callback) = self.on_tempo_map_changed.as_ref() {
            callback(cx);
        } else {
            self.mark_project_changed(cx);
        }
    }

    pub(crate) fn mark_time_signature_map_changed(&self, cx: &mut gpui::App) {
        if let Some(callback) = self.on_time_signature_map_changed.as_ref() {
            callback(cx);
        } else {
            self.mark_project_changed(cx);
        }
    }

    /// Project-key edit: persisted, not part of the engine graph, but read by
    /// ARA plug-ins. Falls back to [`Self::mark_control_state_changed`] (save
    /// dirty only) when no dedicated callback is wired.
    pub(crate) fn mark_musical_context_changed(&self, cx: &mut gpui::App) {
        if let Some(callback) = self.on_musical_context_changed.as_ref() {
            callback(cx);
        } else {
            self.mark_control_state_changed(cx);
        }
    }

    pub(crate) fn mark_loop_changed(&self, cx: &mut gpui::App) {
        if let Some(callback) = self.on_loop_changed.as_ref() {
            callback(cx);
        } else {
            self.mark_project_changed(cx);
        }
    }

    /// Push this tick's levels to the header meters.
    ///
    /// Returns `true` if any meter repainted. As with [`Self::publish_playhead`]
    /// the caller must not follow this with a `Timeline` notify — the whole
    /// point is that a moving meter no longer rebuilds the arrangement.
    ///
    /// Only entities that exist are fed: `render` creates them for the tracks
    /// it draws, so a track scrolled out of view costs nothing here either.
    pub(crate) fn publish_track_meters(&self, cx: &mut gpui::App) -> bool {
        if self.track_meters.is_empty() {
            return false;
        }
        let mut repainted = false;
        for track in &self.state.tracks {
            let Some(meter) = self.track_meters.get(track.id.as_str()) else {
                continue;
            };
            repainted |= meter.update(cx, |meter, cx| {
                meter.apply(track.meter_level_l, track.meter_level_r, cx)
            });
        }
        repainted
    }

    /// Push the current playhead position to its own entity.
    ///
    /// Returns `true` when the overlay actually had to repaint. The caller must
    /// *not* follow this with a `Timeline` notify: moving the playhead is the
    /// one per-frame visual that has no business rebuilding the arrangement,
    /// and re-adding that notify puts the stutter straight back.
    ///
    /// A no-op before the first render, which is when the overlay is built —
    /// the arrangement has not been laid out yet, so there is no x to publish.
    /// Build the arrangement grid layer.
    ///
    /// Lives here rather than in `render` so the cached
    /// [`crate::components::timeline::timeline_surface::TimelineSurfaceView`]
    /// can rebuild it on its own. The row layout is recomputed instead of
    /// borrowed from the frame: it is O(track_count) and only runs when the
    /// grid is actually dirty, which is far less often than every frame.
    /// Build one track's clip lane from this frame's published context.
    ///
    /// Called by the cached [`crate::components::timeline::track_lane_view::TrackLaneView`],
    /// which renders during the prepaint of the tree `render` just returned —
    /// so `frame_lane_ctx` is always this frame's.
    pub(crate) fn render_track_lane(&self, track_id: &str) -> gpui::AnyElement {
        use gpui::{IntoElement, ParentElement, Styled};
        let Some(ctx) = self.frame_lane_ctx.as_ref() else {
            return gpui::div().into_any_element();
        };
        let Some(index) = self.state.tracks.iter().position(|t| t.id == track_id) else {
            return gpui::div().into_any_element();
        };
        let Some(row) = ctx.row_layout.row_for_index(index) else {
            return gpui::div().into_any_element();
        };
        // Anchored in a self-sizing wrapper: `AnyView::cached` lays this out as
        // a *root* with the cached bounds as available space, so the element it
        // renders has no flex context to grow into. `track_lane`'s own root is
        // `flex_1`, which as a root means nothing and leaves the lane at auto
        // width. See `render_arrangement_surface` for the same trap.
        gpui::div()
            .size_full()
            .child(crate::components::timeline::track_lane::track_lane(
                &self.state.tracks[index],
                index,
                &self.state,
                &ctx.gesture,
                row.height,
                ctx.on_select_track.clone(),
                ctx.on_select_clip.clone(),
                ctx.on_add_clip.clone(),
                ctx.on_track_context_menu.clone(),
                ctx.on_clip_context_menu.clone(),
                ctx.on_open_editor.clone(),
                ctx.on_range_start.clone(),
                ctx.on_erase_start.clone(),
                ctx.on_erase_clip.clone(),
                ctx.on_cut_clip.clone(),
                Some(&self.erase_preview_ids),
                ctx.on_audio_clip_process_preview.clone(),
                ctx.on_audio_clip_process_commit.clone(),
                ctx.on_crossfade.clone(),
            ))
            .into_any_element()
    }

    pub(crate) fn render_arrangement_surface(&self) -> gpui::AnyElement {
        use gpui::{IntoElement, ParentElement, Styled};
        let mut row_layout = self.state.track_row_layout();
        row_layout.scroll_y = self.state.viewport.scroll_y;
        let grid_width = self.state.viewport.viewport_width.max(1.0);
        let grid_height = self
            .state
            .viewport
            .viewport_height
            .max(crate::components::timeline::timeline_state::DEFAULT_TRACK_HEIGHT);
        // The renderer's element is `absolute; inset: 0`, which needs a
        // containing block. `AnyView::cached` lays a view's element out as a
        // root, where there is none — the canvas then measures zero and the
        // grid silently stops drawing. The wrapper is that containing block,
        // and it sizes itself from the available space.
        gpui::div()
            .relative()
            .size_full()
            .child(
                crate::components::timeline::timeline_surface::timeline_surface(
                    &self.state,
                    &row_layout,
                    grid_width,
                    grid_height,
                ),
            )
            .into_any_element()
    }

    /// Show the Smart Tool's razor line at `frame`, repainting only the line.
    pub(super) fn show_cut_guide(
        &self,
        frame: crate::components::timeline::cut_guide::CutGuideFrame,
        cx: &mut gpui::App,
    ) {
        let unchanged = self.cut_guide.get().is_some_and(|prev| {
            (prev.x - frame.x).abs() < 0.5
                && (prev.top - frame.top).abs() < 0.5
                && (prev.height - frame.height).abs() < 0.5
        });
        if unchanged {
            return;
        }
        self.cut_guide.set(Some(frame));
        if let Some(overlay) = self.cut_guide_overlay.as_ref() {
            overlay.update(cx, |_, cx| cx.notify());
        }
    }

    pub(crate) fn hide_cut_guide(&self, cx: &mut gpui::App) {
        if self.cut_guide.take().is_some() {
            if let Some(overlay) = self.cut_guide_overlay.as_ref() {
                overlay.update(cx, |_, cx| cx.notify());
            }
        }
    }

    pub(crate) fn publish_playhead(&self, cx: &mut gpui::App) -> bool {
        let Some(overlay) = self.playhead_overlay.as_ref() else {
            return false;
        };
        let next = crate::components::timeline::playhead::PlayheadFrame {
            x: self.state.beats_to_x(self.state.transport.playhead_beats),
        };
        // Sub-pixel motion draws the same line. At 144 Hz on a zoomed-out
        // arrangement most ticks land inside one pixel, and repainting for them
        // is work with nothing to show for it.
        if (self.playhead_frame.get().x - next.x).abs() < 0.5 {
            return false;
        }
        self.playhead_frame.set(next);
        overlay.update(cx, |_, cx| cx.notify());
        true
    }

    pub(crate) fn mark_project_changed(&self, cx: &mut gpui::App) {
        if let Some(callback) = self.on_project_changed.as_ref() {
            callback(cx);
        }
    }

    pub(crate) fn mark_midi_changed(&self, cx: &mut gpui::App) {
        if let Some(callback) = self.on_midi_changed.as_ref() {
            callback(cx);
        } else {
            self.mark_project_changed(cx);
        }
    }

    /// Live mixer-control edit (mute/solo): persisted in the project file but
    /// applied to the engine through the realtime command path, so the owner
    /// must not treat it as an engine-graph change. Also the path for view
    /// state the project file saves (snap, lane visibility, automation
    /// expansion), which never reaches the engine. Falls back to
    /// [`Self::mark_project_changed`] when no dedicated callback is wired.
    pub(crate) fn mark_control_state_changed(&self, cx: &mut gpui::App) {
        if let Some(callback) = self.on_control_state_changed.as_ref() {
            callback(cx);
        } else {
            self.mark_project_changed(cx);
        }
    }

    /// Focus a track's automation editor on `lane_id`. The focused lane of an
    /// expanded track is saved with the project (v54), so a move is an unsaved
    /// change, view-only.
    pub(crate) fn focus_automation_lane(
        &mut self,
        track_id: &str,
        lane_id: &str,
        cx: &mut gpui::App,
    ) {
        let change = self.state.edit_automation_view(track_id, |state| {
            state.activate_automation_lane(track_id, lane_id)
        });
        if change.is_change() {
            self.mark_control_state_changed(cx);
        }
    }

    pub(crate) fn mark_media_changed(&self, cx: &mut gpui::App) {
        if let Some(callback) = self.on_media_changed.as_ref() {
            callback(cx);
        }
    }

    /// The Pen's release: one MIDI clip, default length for a plain click or
    /// the dragged span. Only the Pen draws clips here.
    pub(super) fn finish_pen_midi_clip(&mut self, end_beat: f32, cx: &mut gpui::Context<Self>) {
        use crate::components::timeline::timeline_state::{MIN_MIDI_CLIP_BEATS, TrackType};
        let Some(preview) = self.pen_clip_draw.take() else {
            return;
        };
        let track_id = preview.track_id;
        let track_type = self
            .state
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| t.track_type);
        if !matches!(track_type, Some(TrackType::Midi | TrackType::Instrument)) {
            return;
        }

        let (clip_start, length) = if let Some(range) = self.state.arrangement_range.as_ref() {
            let (range_start, range_end) = range.as_f32_range();
            let (lo, hi) = normalize_range(range_start, range_end);
            (lo, (hi - lo).max(MIN_MIDI_CLIP_BEATS))
        } else {
            // Commit exactly what the ghost preview showed: same start + snapped
            // length helper, fed the live end beat from release.
            compute_pen_clip_span(&self.state, preview.start_beat, end_beat)
        };

        if let Some(clip) = self.state.build_midi_clip(&track_id, clip_start, length) {
            let clip_id = clip.id.clone();
            self.run_edit_command(
                EditCommand::CreateClip {
                    track_id: track_id.clone(),
                    clip,
                },
                cx,
            );
            if crate::components::timeline::timeline_state::midi_debug_enabled() {
                eprintln!(
                    "[midi] clip created track={} clip={} start={:.3} len={:.3}",
                    track_id, clip_id, clip_start, length
                );
            }
        }
    }

    // ── Arrangement marquee ──────────────────────────────────────────────
    // A free rectangle of unsnapped pixels, anchored in arrangement space and
    // followed by window-wide listeners (see `render`), so it keeps tracking
    // outside the timeline and over anything that occludes it. While it is
    // out, the arrangement draws a preview of what it selects; the selection
    // itself changes once, at the release. Selection is UI state: no undo
    // step, and Escape puts the previous selection back.

    /// Start a marquee from a press on lane space no clip owns, or below the
    /// last track.
    pub(super) fn begin_marquee(
        &mut self,
        press: &crate::components::timeline::track_lane::MarqueePress,
        cx: &mut gpui::Context<Self>,
    ) {
        // A marquee whose release never reached the timeline ends here, as a
        // release would, before this one takes its place: its auto-scroll,
        // follow-playhead suspension and preview must not outlive it.
        if self.end_marquee(true).is_some() {
            self.hide_marquee_frame(cx);
        }
        if self.state.active_tool != TimelineTool::Pointer {
            return;
        }
        let selection_before = self.state.selection.clone();
        if press.on_lane && !press.additive {
            // A lane click chooses its track, as it always has; a drag
            // replaces that with what the rectangle encloses. Shown at once,
            // but like the drag's it is a preview until the release commits
            // it, so what follows the selection sees one change per gesture.
            let mut chosen = selection_before.clone();
            chosen.select_track(&press.track_id);
            self.state.marquee_selection_preview = Some(chosen);
        }
        self.state.arrangement_range = None;
        self.marquee_autoscroll = None;
        self.range_select_drag = Some(RangeSelectDrag {
            anchor_beat: self.state.beats_from_window_x(press.window_x),
            anchor_content_y: self.state.content_y_from_window_y(press.window_y),
            press_window: (press.window_x, press.window_y),
            last_window: (press.window_x, press.window_y),
            start_track_id: press.track_id.clone(),
            additive: press.additive,
            on_lane: press.on_lane,
            create_clip_on_click: press.create_clip_on_click,
            bypass_snap: press.bypass_snap,
            dragging: false,
            selection_before,
            hits: None,
        });
        if Self::input_debug_enabled() {
            eprintln!(
                "[selection] marquee_start_pending track={} additive={}",
                press.track_id, press.additive
            );
        }
        cx.notify();
    }

    /// Move the marquee's free corner to the pointer at `window_pos`.
    ///
    /// Every pointer update and the release go through here, so what is
    /// committed is exactly the rectangle that was drawn: no snapping, and no
    /// Shift difference at the release.
    pub(super) fn update_marquee(
        &mut self,
        window_pos: (f32, f32),
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(drag) = self.range_select_drag.as_mut() else {
            return;
        };
        // Same position (macOS repeats the last drag event every 16 ms while
        // the button is held still, and the root and the window-wide tracker
        // both see a move): nothing to do.
        if drag.dragging && drag.last_window == window_pos {
            return;
        }
        drag.last_window = window_pos;
        if !drag.dragging {
            if !crate::components::edit::marquee_drag_started(drag.press_window, window_pos) {
                return;
            }
            drag.dragging = true;
            // Suspend follow-playhead while the rectangle is out, so a Page or
            // Continuous scroll cannot move the arrangement from under it. The
            // user's own Follow choice, shown by the transport and saved in
            // Settings, is left alone.
            self.state.follow_playhead_suspended = true;
            if Self::input_debug_enabled() {
                eprintln!("[selection] marquee_start additive={}", drag.additive);
            }
        }
        self.refresh_marquee(window, cx);
    }

    /// A marquee has ended, however it ended: stop auto-scrolling, lift the
    /// follow-playhead suspension and take the preview down. `commit` makes
    /// the previewed selection the real one — a release, or a marquee that
    /// something else ended — and otherwise drops it (Escape). Returns the
    /// marquee that was in flight.
    pub(super) fn end_marquee(&mut self, commit: bool) -> Option<RangeSelectDrag> {
        let drag = self.range_select_drag.take()?;
        self.marquee_autoscroll = None;
        self.state.follow_playhead_suspended = false;
        let preview = self.state.marquee_selection_preview.take();
        if let Some(selection) = preview.filter(|_| commit) {
            self.state.selection = selection;
        }
        Some(drag)
    }

    /// Re-resolve the rectangle from the last pointer position against the
    /// current scroll and time zoom, preview what it encloses and move its
    /// overlay. Starts auto-scroll when the pointer is at or past the track
    /// area's edge.
    ///
    /// The preview (`TimelineState::marquee_selection_preview`) is what the
    /// arrangement draws as selected. The selection itself — which the
    /// Inspector and the editors follow — changes only when the marquee ends.
    pub(super) fn refresh_marquee(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        use crate::components::timeline::timeline_state::MarqueeRect;
        let Some(drag) = self.range_select_drag.as_ref().filter(|drag| drag.dragging) else {
            return;
        };
        let (window_x, window_y) = drag.last_window;
        let rect = MarqueeRect::from_corners(
            (
                self.state.beats_to_x(drag.anchor_beat),
                drag.anchor_content_y,
            ),
            (
                self.state.lane_x_from_window_x(window_x),
                self.state.content_y_from_window_y(window_y),
            ),
        );
        // This frame's rows: track heights cannot change under a marquee
        // (track zoom waits for it to end).
        let rows = match self.frame_lane_ctx.as_ref() {
            Some(ctx) => ctx.row_layout.clone(),
            None => std::rc::Rc::new(self.state.track_row_layout()),
        };
        let hits = self.state.marquee_hits(&rows, rect);
        if drag.hits.as_ref() != Some(&hits) {
            // From the selection before the press, so an additive marquee is
            // always "before ∪ rectangle" and a replacing one exactly the
            // rectangle, however the rectangle has moved.
            self.state.marquee_selection_preview = Some(drag.selection_before.after_marquee(
                &hits,
                &drag.start_track_id,
                drag.additive,
            ));
            if let Some(drag) = self.range_select_drag.as_mut() {
                drag.hits = Some(hits);
            }
            // The one Timeline notify a marquee makes: the preview changed.
            cx.notify();
        }
        self.show_marquee_frame(
            crate::components::timeline::marquee_overlay::MarqueeFrame {
                left: rect.left,
                top: rect.top - self.state.viewport.scroll_y,
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
            },
            cx,
        );
        if self.marquee_autoscroll.is_none()
            && self.marquee_autoscroll_delta((window_x, window_y)) != (0.0, 0.0)
        {
            self.start_marquee_autoscroll(window, cx);
        }
    }

    /// End the marquee. A release passes its position, which goes through
    /// [`Self::update_marquee`] like every move before the marquee ends.
    ///
    /// A press that never became a drag is a click: the lane's track, shown
    /// as chosen since the press, is committed (an additive click adds it
    /// instead), and a double-click on an empty MIDI / Instrument lane creates
    /// its clip here.
    pub(super) fn finish_marquee(
        &mut self,
        window_pos: Option<(f32, f32)>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(position) = window_pos {
            self.update_marquee(position, window, cx);
        }
        // The release commits the preview — the rectangle's, or a click's
        // track: the one selection change the gesture makes.
        let Some(drag) = self.end_marquee(true) else {
            return;
        };
        self.hide_marquee_frame(cx);
        if !drag.dragging {
            if drag.on_lane && drag.additive {
                self.state.apply_marquee_selection(
                    std::slice::from_ref(&drag.start_track_id),
                    Vec::new(),
                    &drag.start_track_id,
                    true,
                );
            }
            if drag.create_clip_on_click {
                let start = self.snap_beat_with_bypass(drag.anchor_beat, drag.bypass_snap);
                self.create_default_midi_clip(&drag.start_track_id, start, cx);
            }
        }
        if Self::input_debug_enabled() {
            eprintln!(
                "[selection] marquee_commit additive={} dragging={} clips={} tracks={}",
                drag.additive,
                drag.dragging,
                self.state.selection.selected_clip_ids.len(),
                self.state.selection.selected_track_ids.len()
            );
        }
        cx.notify();
    }

    /// Escape during a marquee: drop its preview, put back the selection from
    /// before the press and stop auto-scrolling. Escape reaches the
    /// arrangement only through the Studio's key handler, which calls this
    /// before resetting the rest of the timeline's input state. Returns
    /// whether a marquee was in flight.
    pub fn cancel_marquee(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        let Some(drag) = self.end_marquee(false) else {
            return false;
        };
        self.state.selection = drag.selection_before;
        self.hide_marquee_frame(cx);
        cx.notify();
        true
    }

    /// A command that arrives while a marquee is out (Delete, Copy, a nudge,
    /// any shortcut or menu item) must act on the selection the arrangement
    /// shows. So the marquee ends here as a release would, and its preview
    /// becomes the selection before the command runs. The pointer's later
    /// moves and its release find no marquee and do nothing. Returns whether
    /// a marquee was in flight.
    pub fn finish_marquee_for_command(&mut self, cx: &mut gpui::Context<Self>) -> bool {
        if !self.commit_marquee_for_command() {
            return false;
        }
        self.hide_marquee_frame(cx);
        cx.notify();
        true
    }

    /// The part of [`Self::finish_marquee_for_command`] that needs no
    /// context: end the marquee and commit its preview. A click's own release
    /// effects are dropped with the release the command cut off. Those are an
    /// additive lane click adding its track, which was never previewed, and a
    /// double-click creating a MIDI clip.
    pub(super) fn commit_marquee_for_command(&mut self) -> bool {
        self.end_marquee(true).is_some()
    }

    /// Auto-scroll step for a marquee pointer at `window_pos`, per tick:
    /// `(dx, dy)` in pixels, zero while the pointer is well inside the track
    /// area.
    fn marquee_autoscroll_delta(&self, (x, y): (f32, f32)) -> (f32, f32) {
        use crate::components::timeline::timeline_state::marquee_autoscroll_step;
        let left = self.state.lane_origin_x();
        let top = self.state.timeline_origin_y() + self.state.arrangement_content_top();
        (
            marquee_autoscroll_step(x, left, left + self.state.viewport.viewport_width),
            marquee_autoscroll_step(y, top, top + self.state.viewport.viewport_height),
        )
    }

    /// Scroll on a UI-thread timer while the marquee's pointer is at or past
    /// the track area's edge. Only the timer scrolls — a pointer move never
    /// does — so the speed is the same whether or not the platform repeats
    /// drag events for a pointer held still.
    fn start_marquee_autoscroll(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.marquee_autoscroll = Some(cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let keep = this
                .update_in(cx, |this, window, cx| {
                    this.marquee_autoscroll_tick(window, cx)
                })
                .unwrap_or(false);
            if !keep {
                break;
            }
        }));
    }

    /// One auto-scroll step. `false` once the marquee has ended or the pointer
    /// is back inside the track area, which ends the timer.
    fn marquee_autoscroll_tick(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let Some(pointer) = self
            .range_select_drag
            .as_ref()
            .filter(|drag| drag.dragging)
            .map(|drag| drag.last_window)
        else {
            self.marquee_autoscroll = None;
            return false;
        };
        let (dx, dy) = self.marquee_autoscroll_delta(pointer);
        if (dx, dy) == (0.0, 0.0) {
            self.marquee_autoscroll = None;
            return false;
        }
        let (max_x, max_y) = self.max_scroll_offsets(window);
        let (x, y) = (self.state.viewport.scroll_x, self.state.viewport.scroll_y);
        self.state
            .set_scroll_immediate(x + dx, y + dy, max_x, max_y);
        if (self.state.viewport.scroll_x - x).abs() < 0.01
            && (self.state.viewport.scroll_y - y).abs() < 0.01
        {
            // At the end of the arrangement: wait for the pointer to move back.
            return true;
        }
        // A user-driven scroll, like the wheel's: it turns follow-playhead
        // off for after the marquee (the marquee only suspends it).
        self.state.note_user_scrolled();
        self.refresh_marquee(window, cx);
        cx.notify();
        true
    }

    /// Show the marquee rectangle at `frame`: notifies the overlay only, so
    /// the lanes are not rebuilt (see `marquee_overlay`).
    fn show_marquee_frame(
        &self,
        frame: crate::components::timeline::marquee_overlay::MarqueeFrame,
        cx: &mut gpui::App,
    ) {
        let unchanged = self.marquee_frame.get().is_some_and(|prev| {
            (prev.left - frame.left).abs() < 0.5
                && (prev.top - frame.top).abs() < 0.5
                && (prev.width - frame.width).abs() < 0.5
                && (prev.height - frame.height).abs() < 0.5
        });
        if unchanged {
            return;
        }
        self.marquee_frame.set(Some(frame));
        if let Some(overlay) = self.marquee_overlay.as_ref() {
            overlay.update(cx, |_, cx| cx.notify());
        }
    }

    fn hide_marquee_frame(&self, cx: &mut gpui::App) {
        if self.marquee_frame.take().is_some() {
            if let Some(overlay) = self.marquee_overlay.as_ref() {
                overlay.update(cx, |_, cx| cx.notify());
            }
        }
    }

    /// The Pointer's double-click on an empty MIDI / Instrument lane: one
    /// default-length clip at `start` (the snapped, or Shift-free, press
    /// beat), as one undo step.
    fn create_default_midi_clip(
        &mut self,
        track_id: &str,
        start: f32,
        cx: &mut gpui::Context<Self>,
    ) {
        let (clip_start, length) = compute_pen_clip_span(&self.state, start, start);
        if let Some(clip) = self.state.build_midi_clip(track_id, clip_start, length) {
            self.run_edit_command(
                EditCommand::CreateClip {
                    track_id: track_id.to_string(),
                    clip,
                },
                cx,
            );
        }
    }

    /// Whether a pointer gesture is in flight that a Smart Tool cut must not
    /// interrupt: a marquee, a pen draw, a right-drag erase or a clip move.
    pub(super) fn pointer_gesture_blocks_cut(&self) -> bool {
        self.range_select_drag.is_some()
            || self.pen_clip_draw.is_some()
            || self.erase_clip_drag.is_some()
            || self.clip_drag_origin.is_some()
            || self.clip_move_origin.is_some()
    }

    /// Show or hide the Smart Tool's razor line for a cut-zone hover. It
    /// shows only while the cut's modifier is held — Option alone, the same
    /// test the click makes — at the snapped split position.
    pub(super) fn apply_cut_hover(
        &mut self,
        hover: crate::components::timeline::audio_clip::ClipCutGesture,
        cx: &mut gpui::App,
    ) {
        use crate::components::timeline::audio_clip::ClipCutGesture;
        let ClipCutGesture::Hover {
            window_x,
            bypass_snap,
            armed,
            left,
            right,
            top,
            height,
        } = hover
        else {
            return;
        };
        self.cut_hover = Some(hover);
        if !armed || self.pointer_gesture_blocks_cut() {
            self.hide_cut_guide(cx);
            return;
        }
        // Same transform and snap as the cut, so the line is where the click
        // will split.
        let beat = self.beat_from_window_x(window_x);
        let snapped = self.snap_beat_with_bypass(beat, bypass_snap);
        let x = (window_x + self.state.beats_to_x(snapped) - self.state.beats_to_x(beat))
            .clamp(left, right.max(left));
        self.show_cut_guide(
            crate::components::timeline::cut_guide::CutGuideFrame { x, top, height },
            cx,
        );
    }

    /// The Studio forwards every modifier change: pressing or releasing Option
    /// (or Shift, which bypasses snap) while the pointer rests in a cut zone
    /// shows, hides or re-snaps the razor line without waiting for a move.
    pub fn smart_cut_modifiers_changed(
        &mut self,
        modifiers: &gpui::Modifiers,
        cx: &mut gpui::Context<Self>,
    ) {
        use crate::components::timeline::audio_clip::{smart_cut_modifier_held, ClipCutGesture};
        let Some(ClipCutGesture::Hover {
            window_x,
            left,
            right,
            top,
            height,
            ..
        }) = self.cut_hover.clone()
        else {
            return;
        };
        self.apply_cut_hover(
            ClipCutGesture::Hover {
                window_x,
                bypass_snap: modifiers.shift,
                armed: smart_cut_modifier_held(modifiers),
                left,
                right,
                top,
                height,
            },
            cx,
        );
    }

    pub(super) fn finish_erase_clip_drag(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(erased) = self.erase_clip_drag.take() else {
            return;
        };
        self.erase_preview_ids.clear();
        if erased.is_empty() {
            return;
        }
        let snapshots: Vec<ClipSnapshot> = erased
            .iter()
            .filter_map(|id| ClipSnapshot::capture(&self.state, id))
            .collect();
        if snapshots.is_empty() {
            return;
        }
        self.erase_preview_ids.clear();
        self.run_edit_command(EditCommand::BatchDeleteClips { snapshots }, cx);
    }

    /// Extend a right-drag erase with the clips under the pointer at
    /// `window_pos` — on the track under the pointer only, where the clips are
    /// drawn. It used to take every clip on every track at that beat, so a
    /// right-drag that began on one clip could delete a whole column of the
    /// arrangement.
    pub(super) fn update_erase_clip_drag(
        &mut self,
        window_pos: gpui::Point<gpui::Pixels>,
        cx: &mut gpui::Context<Self>,
    ) {
        let lane_x = self.state.lane_x_from_window_x(window_pos.x.into());
        let content_y = self.state.content_y_from_window_y(window_pos.y.into());
        let rows = match self.frame_lane_ctx.as_ref() {
            Some(ctx) => ctx.row_layout.clone(),
            None => std::rc::Rc::new(self.state.track_row_layout()),
        };
        let ids = self.state.clips_at_lane_point(&rows, lane_x, content_y);
        let set = self.erase_clip_drag.get_or_insert_with(HashSet::new);
        let before = set.len();
        set.extend(ids);
        if set.len() != before {
            self.erase_preview_ids = set.clone();
            cx.notify();
        }
    }

    pub(super) fn begin_erase_at(&mut self, clip_id: Option<String>, cx: &mut gpui::Context<Self>) {
        // A known clip (right-click erase) seeds the set with exactly that
        // clip. The drag that follows extends it only with clips under the
        // pointer on the track under the pointer.
        let mut set = HashSet::new();
        if let Some(id) = clip_id {
            set.insert(id);
        }
        self.erase_preview_ids = set.clone();
        self.erase_clip_drag = Some(set);
        cx.notify();
    }

    // ── Automation lane interaction ──────────────────────────────────────────
    // Add/select/move/marquee/delete of automation points. Selection + marquee
    // are UI-only; point add/move commit dirty exactly once on mouse release.

    /// Map a window-space y to a lane-local automation value for `track_id`.
    pub(super) fn automation_value_from_window_y(
        &self,
        track_id: &str,
        lane_id: &str,
        window_y: f32,
    ) -> f32 {
        use crate::components::timeline::timeline_state::{
            AUTOMATION_SUBLANE_HEIGHT, automation_y_to_value,
        };
        // Map against the lane's own sub-row bounds so a drag stays anchored to
        // the lane the gesture started in.
        let (lane_y, lane_h) = self
            .state
            .automation_sublane_geometry(track_id, lane_id)
            .unwrap_or((0.0, AUTOMATION_SUBLANE_HEIGHT));
        let local_y = self.state.content_y_from_window_y(window_y) - lane_y;
        automation_y_to_value(local_y, lane_h)
    }

    pub(super) fn tempo_bpm_from_window_y(&self, window_y: f32) -> f64 {
        self.state.tempo_bpm_at_window_y(window_y)
    }

    // ── Marker lane ─────────────────────────────────────────────────────────

    /// Mouse-down in the Marker lane. Hitting a flag selects it and seeks there;
    /// hitting empty lane clears the selection and seeks; a double-click on
    /// empty lane creates a marker.
    pub(super) fn begin_marker_track_interaction(
        &mut self,
        down: &crate::components::timeline::marker_track::MarkerLaneDown,
        cx: &mut Context<Self>,
    ) {
        match down.marker_id.clone() {
            Some(id) => {
                self.state.select_marker(&id);
                if let Some(marker) = self.state.marker(&id) {
                    let target = marker.beat as f32;
                    // The grab offset is captured against the marker's real
                    // beat before the seek, so the flag keeps its position
                    // under the cursor for the whole gesture.
                    self.marker_drag = Some(TimelineMarkerDrag {
                        marker_id: id.clone(),
                        press_lane_x: down.lane_x,
                        grab_offset_beats: down.pointer_beat - marker.beat,
                        moved: false,
                    });
                    self.seek_to_exact_beat(target, crate::layout::SeekReason::TimelineClick, cx);
                }
            }
            None => {
                self.state.clear_marker_selection();
                if down.click_count >= 2 {
                    let prev = self.state.markers.clone();
                    let id = self.state.add_marker_at_beat(down.snapped_beat);
                    self.state.select_marker(&id);
                    self.record_marker_edit("Add Marker", prev, cx);
                } else {
                    self.seek_to_exact_beat(
                        down.snapped_beat as f32,
                        crate::layout::SeekReason::TimelineClick,
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    /// One frame of the Marker lane gesture, driven by the timeline root's
    /// mouse-move listener.
    ///
    /// Returns `true` once the gesture has passed the drag threshold, so the
    /// caller can stop treating the pointer as a plain click.
    pub(super) fn update_marker_track_interaction(
        &mut self,
        window_x: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.marker_drag.clone() else {
            return false;
        };
        let lane_x = self.state.lane_x_from_window_x(window_x);
        if !drag.moved
            && (lane_x - drag.press_lane_x).abs()
                < crate::components::timeline::timeline_state::CONDUCTOR_DRAG_THRESHOLD_PX
        {
            return false;
        }
        let snapped = self.state.marker_drag_beat(lane_x, drag.grab_offset_beats);
        if let Some(session) = self.marker_drag.as_mut() {
            session.moved = true;
        }
        self.update_marker_drag(&drag.marker_id, snapped, cx);
        true
    }

    /// Abandon the Marker lane gesture and put the marker back where it started.
    ///
    /// Escape during a move has to restore, not just stop: dropping the
    /// snapshot would leave the marker at the last previewed beat with no
    /// history entry describing how it got there.
    pub(super) fn cancel_marker_track_interaction(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(_) = self.marker_drag.take() else {
            return false;
        };
        if let Some(prev) = self.marker_gesture_origin.take() {
            self.state.markers = prev;
            self.state.sort_markers();
        }
        cx.notify();
        true
    }

    /// End the Marker lane gesture, recording one undo entry for the whole move.
    pub(super) fn finish_marker_track_interaction(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(drag) = self.marker_drag.take() else {
            return false;
        };
        if drag.moved {
            self.finish_marker_drag(cx);
        } else {
            // A plain click never opened a history entry, so drop the snapshot
            // rather than writing a no-op step.
            self.marker_gesture_origin = None;
        }
        cx.notify();
        true
    }

    /// Add a marker at the playhead from the lane header's + button.
    pub(super) fn add_marker_at_playhead_from_header(&mut self, cx: &mut Context<Self>) {
        let prev = self.state.markers.clone();
        let beat = self.state.transport.playhead_beats.max(0.0) as f64;
        let id = self.state.add_marker_at_beat(beat);
        self.state.select_marker(&id);
        self.record_marker_edit("Add Marker", prev, cx);
        cx.notify();
    }

    /// One frame of a Marker lane flag drag. Mutates live; the drop records the
    /// single history entry for the whole gesture.
    pub(super) fn update_marker_drag(
        &mut self,
        marker_id: &str,
        beat: f64,
        cx: &mut Context<Self>,
    ) {
        if self.marker_gesture_origin.is_none() {
            self.marker_gesture_origin = Some(self.state.markers.clone());
        }
        if self.state.move_marker(marker_id, beat) {
            self.state.select_marker(marker_id);
            self.mark_project_changed(cx);
            cx.notify();
        }
    }

    pub(super) fn finish_marker_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(prev) = self.marker_gesture_origin.take() {
            self.record_marker_edit("Move Marker", prev, cx);
        }
        cx.notify();
    }

    // ── Region lane ─────────────────────────────────────────────────────────

    /// Mouse-down in the Region lane. Mirrors the Marker lane: a hit selects and
    /// seeks to the section start, empty lane clears, double-click creates.
    pub(super) fn begin_region_track_interaction(
        &mut self,
        beat: f64,
        region_id: Option<String>,
        click_count: u32,
        cx: &mut Context<Self>,
    ) {
        match region_id {
            Some(id) => {
                self.state.select_region(&id);
                if let Some(region) = self.state.region(&id) {
                    let target = region.normalized_range().0 as f32;
                    self.seek_to_exact_beat(target, crate::layout::SeekReason::TimelineClick, cx);
                }
            }
            None => {
                self.state.clear_region_selection();
                if click_count >= 2 {
                    let prev = self.state.regions.clone();
                    let id = self.state.add_region_at_beat(beat);
                    self.state.select_region(&id);
                    self.record_region_edit("Add Region", prev, cx);
                } else {
                    self.seek_to_exact_beat(
                        beat as f32,
                        crate::layout::SeekReason::TimelineClick,
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    /// Chord Track mouse-down: a chord selects and seeks to it; the empty
    /// lane seeks, and a double-click there opens the Chord Generator.
    pub(super) fn begin_chord_track_interaction(
        &mut self,
        beat: f64,
        event_id: Option<u64>,
        click_count: u32,
        cx: &mut Context<Self>,
    ) {
        match event_id {
            Some(id) => {
                self.state.select_chord_event(id);
                if let Some(event) = self.state.chord_event(id) {
                    let target = event.start_beat as f32;
                    self.seek_to_exact_beat(target, crate::layout::SeekReason::TimelineClick, cx);
                }
            }
            None => {
                self.state.clear_chord_selection();
                if click_count >= 2 {
                    self.request_command("chords:open-generator", cx);
                } else {
                    self.seek_to_exact_beat(
                        beat as f32,
                        crate::layout::SeekReason::TimelineClick,
                        cx,
                    );
                }
            }
        }
        cx.notify();
    }

    pub(super) fn add_region_at_playhead_from_header(&mut self, cx: &mut Context<Self>) {
        let prev = self.state.regions.clone();
        let beat = self.state.transport.playhead_beats.max(0.0) as f64;
        let id = self.state.add_region_at_beat(beat);
        self.state.select_region(&id);
        self.record_region_edit("Add Region", prev, cx);
        cx.notify();
    }

    pub(super) fn begin_tempo_track_interaction(
        &mut self,
        down: &crate::components::timeline::tempo_track::TempoLaneDown,
        cx: &mut Context<Self>,
    ) {
        use crate::components::timeline::timeline_state::{
            TempoCurve, TempoCurveDrag, TempoLaneDrag, TempoPointDrag,
        };
        let (beat, bpm, click_count) = (down.beat, down.bpm, down.click_count);

        // On the drawn line between two markers: bend that ramp. Alt +
        // double-click straightens it again.
        if let (Some(left_id), None) = (down.segment_left_id.clone(), down.point_id.as_ref()) {
            if click_count >= 2 && down.alt {
                let origin = self.capture_tempo_state();
                let anchors = self.state.capture_linear_clip_anchors();
                if self.state.set_tempo_point_tension(&left_id, 0.0) {
                    self.state.reconcile_audio_clip_lengths();
                    self.state.reapply_linear_clip_anchors(&anchors);
                    self.record_tempo_edit("Straighten Tempo Ramp", origin, cx);
                }
                cx.notify();
                return;
            }
            if click_count < 2 {
                let points = &self.state.tempo_map.points;
                if let Some(index) = points.iter().position(|p| p.id == left_id) {
                    if let Some(next) = points.get(index + 1) {
                        let left = &points[index];
                        let start_tension = if left.curve == TempoCurve::Linear {
                            left.tension
                        } else {
                            0.0
                        };
                        let rising = next.bpm >= left.bpm;
                        self.state.select_tempo_point(&left_id);
                        self.tempo_gesture_origin =
                            Some(("Bend Tempo Ramp", self.capture_tempo_state()));
                        self.tempo_gesture_linear_anchors =
                            self.state.capture_linear_clip_anchors();
                        self.tempo_drag = Some(TempoLaneDrag::Curve(TempoCurveDrag {
                            left_id,
                            start_tension,
                            start_window_y: down.window_y,
                            rising,
                            changed: false,
                        }));
                        cx.notify();
                        return;
                    }
                }
            }
        }

        if click_count >= 2 {
            if down.point_id.is_none() {
                let origin = self.capture_tempo_state();
                self.tempo_gesture_linear_anchors = self.state.capture_linear_clip_anchors();
                if let Some(id) = self.state.add_tempo_point(beat, bpm) {
                    self.state.select_tempo_point(&id);
                    // The double-click both creates the marker and starts a
                    // drag: keep the pre-create snapshot as the gesture origin
                    // so the release records ONE entry covering both.
                    self.tempo_gesture_origin = Some(("Add Tempo Marker", origin));
                    self.tempo_drag = Some(TempoLaneDrag::Point(TempoPointDrag {
                        point_id: id,
                        moved: true,
                    }));
                }
            }
            cx.notify();
            return;
        }

        if let Some(id) = down.point_id.clone() {
            self.state.select_tempo_point(&id);
            self.tempo_gesture_origin = Some(("Move Tempo Marker", self.capture_tempo_state()));
            self.tempo_gesture_linear_anchors = self.state.capture_linear_clip_anchors();
            self.tempo_drag = Some(TempoLaneDrag::Point(TempoPointDrag {
                point_id: id,
                moved: false,
            }));
            cx.notify();
            return;
        }

        self.state.clear_tempo_point_selection();
        cx.notify();
    }

    pub(super) fn update_tempo_track_interaction(
        &mut self,
        window_x: f32,
        window_y: f32,
        fine: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::components::timeline::timeline_state::{
            tempo_drag_tension, TempoCurve, TempoLaneDrag,
        };
        match self.tempo_drag.clone() {
            Some(TempoLaneDrag::Point(drag)) => {
                let beat = self.snap_beat(self.beat_from_window_x(window_x)).max(0.0) as f64;
                let bpm = self.tempo_bpm_from_window_y(window_y);
                if self.state.move_tempo_point(&drag.point_id, beat, bpm) {
                    if let Some(TempoLaneDrag::Point(d)) = self.tempo_drag.as_mut() {
                        d.moved = true;
                    }
                    cx.notify();
                    true
                } else {
                    false
                }
            }
            Some(TempoLaneDrag::Curve(drag)) => {
                let lane_height = self.state.tempo_track_height();
                let tension = tempo_drag_tension(&drag, window_y, lane_height, fine);
                // A bend is a ramp: a stepped or eased segment grabbed and
                // dragged becomes a Linear ramp carrying the bend.
                let curve_changed = self
                    .state
                    .tempo_map
                    .points
                    .iter()
                    .find(|p| p.id == drag.left_id)
                    .is_some_and(|p| p.curve != TempoCurve::Linear);
                if curve_changed && (drag.start_window_y - window_y).abs() < 2.0 {
                    return false;
                }
                if curve_changed {
                    self.state
                        .set_tempo_point_curve(&drag.left_id, TempoCurve::Linear);
                }
                self.state.set_tempo_point_tension(&drag.left_id, tension);
                if let Some(TempoLaneDrag::Curve(d)) = self.tempo_drag.as_mut() {
                    d.changed = true;
                }
                cx.notify();
                true
            }
            None => false,
        }
    }

    pub(super) fn finish_tempo_track_interaction(&mut self, cx: &mut Context<Self>) -> bool {
        use crate::components::timeline::timeline_state::TempoLaneDrag;
        if let Some(drag) = self.tempo_drag.take() {
            let origin = self.tempo_gesture_origin.take();
            let anchors = std::mem::take(&mut self.tempo_gesture_linear_anchors);
            let changed = match drag {
                TempoLaneDrag::Point(drag) => drag.moved,
                TempoLaneDrag::Curve(drag) => drag.changed,
            };
            if changed {
                // The map moved under the arrangement, so settle the clips
                // against it before the edit is recorded: audio clips re-derive
                // their bar count from the audio they play, and Linear-timebase
                // clips go back to the wall-clock position they were dragged
                // from. Same two passes as a menu-driven tempo edit and as undo.
                self.state.reconcile_audio_clip_lengths();
                self.state.reapply_linear_clip_anchors(&anchors);
                match origin {
                    Some((label, prev)) => {
                        self.record_tempo_edit(label, prev, cx);
                    }
                    None => self.mark_tempo_map_changed(cx),
                }
            }
            cx.notify();
            true
        } else {
            false
        }
    }

    pub(super) fn begin_time_signature_track_interaction(
        &mut self,
        beat: f64,
        point_id: Option<String>,
        click_count: u32,
        cx: &mut Context<Self>,
    ) {
        if click_count >= 2 {
            if let Some(id) = point_id {
                self.state.select_time_signature_point(&id);
            } else {
                let pt = self.state.time_signature_map.time_signature_at_beat(beat);
                let origin = self.capture_time_signature_state();
                if let Some(id) =
                    self.state
                        .add_time_signature_point(beat, pt.numerator, pt.denominator)
                {
                    self.state.select_time_signature_point(&id);
                    self.ts_gesture_origin = Some(("Add Time Signature", origin));
                    self.ts_drag = Some(TimeSignaturePointDrag {
                        point_id: id,
                        moved: true,
                    });
                }
            }
            cx.notify();
            return;
        }

        if let Some(id) = point_id {
            self.state.select_time_signature_point(&id);
            self.ts_gesture_origin =
                Some(("Move Time Signature", self.capture_time_signature_state()));
            self.ts_drag = Some(TimeSignaturePointDrag {
                point_id: id,
                moved: false,
            });
            cx.notify();
            return;
        }

        self.state.clear_time_signature_point_selection();
        cx.notify();
    }

    pub(super) fn update_time_signature_track_interaction(
        &mut self,
        window_x: f32,
        _window_y: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.ts_drag.clone() else {
            return false;
        };
        let beat = self.snap_beat(self.beat_from_window_x(window_x)).max(0.0) as f64;
        if self.state.move_time_signature_point(&drag.point_id, beat) {
            if let Some(d) = self.ts_drag.as_mut() {
                d.moved = true;
            }
            cx.notify();
            true
        } else {
            false
        }
    }

    pub(super) fn add_tempo_point_at_playhead_from_header(&mut self, cx: &mut Context<Self>) {
        let beat = self.state.transport.playhead_beats as f64;
        let bpm = self.state.effective_bpm_at_beat(beat);
        let prev = self.capture_tempo_state();
        if let Some(id) = self.state.add_tempo_point(beat, bpm) {
            self.state.select_tempo_point(&id);
            self.record_tempo_edit("Add Tempo Marker", prev, cx);
            cx.notify();
        }
    }

    pub(super) fn add_time_signature_marker_at_playhead_from_header(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let beat = self.state.transport.playhead_beats as f64;
        let pt = self.state.time_signature_map.time_signature_at_beat(beat);
        let prev = self.capture_time_signature_state();
        if let Some(id) = self
            .state
            .add_time_signature_point(beat, pt.numerator, pt.denominator)
        {
            self.state.select_time_signature_point(&id);
            self.record_time_signature_edit("Add Time Signature", prev, cx);
            cx.notify();
        }
    }

    pub(super) fn finish_time_signature_track_interaction(
        &mut self,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(drag) = self.ts_drag.take() {
            let origin = self.ts_gesture_origin.take();
            if drag.moved {
                match origin {
                    Some((label, prev)) => {
                        self.record_time_signature_edit(label, prev, cx);
                    }
                    None => self.mark_time_signature_map_changed(cx),
                }
            }
            cx.notify();
            true
        } else {
            false
        }
    }

    /// Mouse-down inside an automation lane: hit-test a point (select + begin
    /// move), else add a point (Pen) or start a marquee (Pointer).
    #[allow(clippy::too_many_arguments)]
    /// Delete the automation point under the cursor. Returns `true` if one was
    /// there, which is what tells the lane whether to swallow the click.
    ///
    /// Same hit tolerances as [`Self::begin_automation_interaction`], but the
    /// caller passes the *unsnapped* beat: a delete must act on the point the
    /// cursor is actually over, and snapping the probe to the grid first would
    /// step past a point that sits between grid lines. Anywhere else the press
    /// is left alone and the arrangement's context menu opens as before.
    pub(super) fn delete_automation_point_at(
        &mut self,
        track_id: &str,
        lane_id: &str,
        beat: f32,
        value: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::components::timeline::timeline_state::{
            AUTOMATION_LANE_PAD, AUTOMATION_SUBLANE_HEIGHT,
        };
        let ppb = self.state.viewport.pixels_per_beat.max(1.0);
        let usable = (AUTOMATION_SUBLANE_HEIGHT - 2.0 * AUTOMATION_LANE_PAD).max(1.0);
        let Some(point_id) =
            self.state
                .automation_point_at(track_id, lane_id, beat, value, 8.0 / ppb, 8.0 / usable)
        else {
            return false;
        };
        // Captured before the removal: undo has to put the point back, and on
        // the last point of a lane it has to put the lane back too.
        let lanes_before = self.state.capture_automation_lanes(track_id);
        if !self
            .state
            .delete_automation_point(track_id, lane_id, point_id)
        {
            return false;
        }
        self.record_automation_lanes_edit(track_id, lanes_before, cx);
        cx.notify();
        true
    }

    pub(super) fn begin_automation_interaction(
        &mut self,
        track_id: &str,
        lane_id: &str,
        beat: f32,
        value: f32,
        additive: bool,
        alt: bool,
        double_click: bool,
        cx: &mut Context<Self>,
    ) {
        use crate::components::timeline::timeline_state::{
            AUTOMATION_LANE_PAD, AUTOMATION_SUBLANE_HEIGHT, AutomationCurveDrag, AutomationHover,
            AutomationMarquee, AutomationPointDrag, TrackLaneMode,
        };
        self.state.select_track(track_id);
        if self.state.track_lane_mode(track_id) != TrackLaneMode::Automation {
            return;
        }
        // Undo baseline for whatever this gesture turns out to be. Captured
        // before any mutation because drawing the first point *creates* the
        // lane, and undo has to remove it again.
        let lanes_before = self.state.capture_automation_lanes(track_id);
        // Focus the editor on the clicked lane and make sure it exists.
        self.focus_automation_lane(track_id, lane_id, cx);
        if self.state.automation_lane(track_id, lane_id).is_none() {
            return;
        }
        let lane_id = lane_id.to_string();

        let ppb = self.state.viewport.pixels_per_beat.max(1.0);
        let usable = (AUTOMATION_SUBLANE_HEIGHT - 2.0 * AUTOMATION_LANE_PAD).max(1.0);
        let beat_tol = 8.0 / ppb;
        let value_tol = 8.0 / usable;

        if let Some(point_id) = self
            .state
            .automation_point_at(track_id, &lane_id, beat, value, beat_tol, value_tol)
        {
            // Select (UI-only) and begin a move drag.
            self.state
                .select_automation_point(track_id, &lane_id, point_id, additive);
            self.automation_drag = Some(AutomationPointDrag {
                track_id: track_id.to_string(),
                lane_id,
                point_id,
                moved: false,
                undo_before: lanes_before,
            });
            cx.notify();
            return;
        }

        // Curve-segment editing on the curve line (not a point, checked above), so
        // it never disturbs point add / move / marquee selection. It fires when
        // either Alt is held (explicit modifier, works in any tool) or the Pointer
        // tool is active (direct edit — the Pointer tool's plain drag on the lane
        // would otherwise only marquee). Pen/Automation keep plain-click = add a
        // point so curve drawing still works; Alt shapes tension there.
        let curve_edit = alt || self.state.active_tool == TimelineTool::Pointer;
        if curve_edit {
            if let Some(left_id) = self.state.automation_segment_left_point_at(
                track_id,
                &lane_id,
                beat,
                value,
                value_tol * 1.5,
            ) {
                if double_click {
                    if self
                        .state
                        .reset_automation_segment_curve(track_id, &lane_id, left_id)
                    {
                        self.record_automation_lanes_edit(track_id, lanes_before.clone(), cx);
                    }
                    self.automation_hover = Some(AutomationHover {
                        track_id: track_id.to_string(),
                        lane_id,
                        point_id: None,
                        segment_left_id: Some(left_id),
                        active: false,
                    });
                } else {
                    let start_tension = self
                        .state
                        .automation_segment_tension(track_id, &lane_id, left_id);
                    self.automation_curve_drag = Some(AutomationCurveDrag {
                        track_id: track_id.to_string(),
                        lane_id: lane_id.clone(),
                        left_point_id: left_id,
                        start_tension,
                        start_value: value,
                        changed: false,
                        undo_before: lanes_before.clone(),
                    });
                    // Mark the segment active so the renderer shows the strong
                    // drag highlight (hover updates are suppressed during a drag).
                    self.automation_hover = Some(AutomationHover {
                        track_id: track_id.to_string(),
                        lane_id,
                        point_id: None,
                        segment_left_id: Some(left_id),
                        active: true,
                    });
                }
                cx.notify();
                return;
            }
        }

        match self.state.active_tool {
            TimelineTool::Pen | TimelineTool::Automation => {
                // Add a point and begin dragging it. The commit happens once on
                // release (moved=true), so a plain click still persists the add.
                if !additive {
                    self.state.clear_automation_selection(track_id);
                }
                if let Some(point_id) = self
                    .state
                    .add_automation_point(track_id, &lane_id, beat, value)
                {
                    self.state
                        .select_automation_point(track_id, &lane_id, point_id, false);
                    self.automation_drag = Some(AutomationPointDrag {
                        track_id: track_id.to_string(),
                        lane_id,
                        point_id,
                        moved: true,
                        undo_before: lanes_before,
                    });
                }
                cx.notify();
            }
            _ => {
                // Pointer (and other tools): rubber-band marquee selection.
                if !additive {
                    self.state.clear_automation_selection(track_id);
                }
                self.automation_marquee = Some(AutomationMarquee {
                    track_id: track_id.to_string(),
                    lane_id,
                    start_beat: beat,
                    start_value: value,
                    cur_beat: beat,
                    cur_value: value,
                    additive,
                });
                cx.notify();
            }
        }
    }

    /// Live update during an automation drag or marquee. Returns true if a
    /// gesture was active and consumed the move.
    pub(super) fn update_automation_interaction(
        &mut self,
        window_x: f32,
        window_y: f32,
        fine: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(drag) = self.automation_drag.clone() {
            let beat = self.snap_beat(self.beat_from_window_x(window_x)).max(0.0);
            let value =
                self.automation_value_from_window_y(&drag.track_id, &drag.lane_id, window_y);
            self.state.move_automation_point(
                &drag.track_id,
                &drag.lane_id,
                drag.point_id,
                beat,
                value,
            );
            if let Some(d) = self.automation_drag.as_mut() {
                d.moved = true;
            }
            cx.notify();
            return true;
        }
        if let Some(drag) = self.automation_curve_drag.clone() {
            // Vertical drag distance from the grab point maps to a tension delta;
            // the points never move. Dragging up raises tension (ease-in), down
            // lowers it (ease-out); clamped to the safe range by the setter. Shift
            // = fine adjust (quarter gain) for precise shaping.
            let value =
                self.automation_value_from_window_y(&drag.track_id, &drag.lane_id, window_y);
            const TENSION_GAIN: f32 = 2.4;
            const TENSION_GAIN_FINE: f32 = 0.6;
            let gain = if fine {
                TENSION_GAIN_FINE
            } else {
                TENSION_GAIN
            };
            let tension = (drag.start_tension + (value - drag.start_value) * gain).clamp(-1.0, 1.0);
            self.state.set_automation_segment_tension(
                &drag.track_id,
                &drag.lane_id,
                drag.left_point_id,
                tension,
            );
            if let Some(d) = self.automation_curve_drag.as_mut() {
                d.changed = true;
            }
            cx.notify();
            return true;
        }
        if let Some(mut m) = self.automation_marquee.clone() {
            let beat = self.beat_from_window_x(window_x).max(0.0);
            let value = self.automation_value_from_window_y(&m.track_id, &m.lane_id, window_y);
            m.cur_beat = beat;
            m.cur_value = value;
            self.state.marquee_select_automation(
                &m.track_id,
                &m.lane_id,
                m.start_beat,
                beat,
                m.start_value,
                value,
                m.additive,
            );
            self.automation_marquee = Some(m);
            cx.notify();
            return true;
        }
        false
    }

    /// Commit an automation gesture on mouse release. Point moves/adds dirty the
    /// project exactly once; marquee selection is UI-only. Returns true if a
    /// gesture was active.
    pub(super) fn finish_automation_interaction(&mut self, cx: &mut Context<Self>) -> bool {
        let mut handled = false;
        if let Some(drag) = self.automation_drag.take() {
            if drag.moved {
                // One history entry for the whole drag: the points were mutated
                // live, so record the already-applied result rather than
                // re-executing it.
                self.record_automation_lanes_edit(&drag.track_id, drag.undo_before, cx);
            }
            handled = true;
        }
        if let Some(drag) = self.automation_curve_drag.take() {
            if drag.changed {
                self.record_automation_lanes_edit(&drag.track_id, drag.undo_before, cx);
            }
            // Relax the strong drag highlight back to plain hover; the cursor is
            // still on the segment, so keep it hovered (next move re-tests).
            if let Some(hover) = self.automation_hover.as_mut() {
                hover.active = false;
            }
            handled = true;
        }
        if self.automation_marquee.take().is_some() {
            handled = true;
        }
        if handled {
            cx.notify();
        }
        handled
    }

    /// Update the hovered automation point / segment for `(track, lane)` from a
    /// mouse-move at `(beat, value)`. Point hover wins over segment hover, and the
    /// hit-test order/tolerances mirror [`Self::begin_automation_interaction`] so
    /// the hover affordance always matches what a click would actually grab.
    /// UI-only: notifies only when the hovered target changes (an idle move over
    /// the same segment never repaints), and no-ops while a gesture owns the lane.
    pub(super) fn update_automation_hover(
        &mut self,
        track_id: &str,
        lane_id: &str,
        beat: f32,
        value: f32,
        cx: &mut Context<Self>,
    ) {
        use crate::components::timeline::timeline_state::{
            AUTOMATION_LANE_PAD, AUTOMATION_SUBLANE_HEIGHT, AutomationHover, TrackLaneMode,
        };
        if self.automation_drag.is_some()
            || self.automation_curve_drag.is_some()
            || self.automation_marquee.is_some()
        {
            return;
        }
        if self.state.track_lane_mode(track_id) != TrackLaneMode::Automation {
            self.clear_automation_hover(cx);
            return;
        }
        let ppb = self.state.viewport.pixels_per_beat.max(1.0);
        let usable = (AUTOMATION_SUBLANE_HEIGHT - 2.0 * AUTOMATION_LANE_PAD).max(1.0);
        let beat_tol = 8.0 / ppb;
        let value_tol = 8.0 / usable;
        let point_id = self
            .state
            .automation_point_at(track_id, lane_id, beat, value, beat_tol, value_tol);
        // Point priority: only test the segment when not already on a point.
        let segment_left_id = if point_id.is_some() {
            None
        } else {
            self.state.automation_segment_left_point_at(
                track_id,
                lane_id,
                beat,
                value,
                value_tol * 1.5,
            )
        };
        // Compare before building so an idle move over the same target allocates
        // nothing (no String clones) and does not repaint — only a changed hover
        // target builds + stores a new `AutomationHover`.
        let unchanged = match self.automation_hover.as_ref() {
            Some(h) => {
                h.matches_lane(track_id, lane_id)
                    && h.point_id == point_id
                    && h.segment_left_id == segment_left_id
                    && !h.active
            }
            None => point_id.is_none() && segment_left_id.is_none(),
        };
        if unchanged {
            return;
        }
        self.automation_hover =
            (point_id.is_some() || segment_left_id.is_some()).then(|| AutomationHover {
                track_id: track_id.to_string(),
                lane_id: lane_id.to_string(),
                point_id,
                segment_left_id,
                active: false,
            });
        cx.notify();
    }

    /// Clear automation hover (cursor left the lane). Keeps the highlight while a
    /// curve drag is active even if the cursor strays out of the lane bounds.
    /// Notifies only when something actually changed.
    pub(super) fn clear_automation_hover(&mut self, cx: &mut Context<Self>) {
        if self.automation_curve_drag.is_some() {
            return;
        }
        if self.automation_hover.take().is_some() {
            cx.notify();
        }
    }

    /// Clear hover only if it currently targets `(track, lane)` — the cursor left
    /// that specific lane. Lane-scoped so leaving lane A never wipes a fresh hover
    /// the cursor just established on lane B during a fast move.
    pub(super) fn clear_automation_hover_for_lane(
        &mut self,
        track_id: &str,
        lane_id: &str,
        cx: &mut Context<Self>,
    ) {
        if self.automation_curve_drag.is_some() {
            return;
        }
        if self
            .automation_hover
            .as_ref()
            .is_some_and(|h| h.matches_lane(track_id, lane_id))
        {
            self.automation_hover = None;
            cx.notify();
        }
    }

    pub(super) fn timeline_content_width(&self) -> f32 {
        let clip_end_seconds = self
            .state
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .map(|clip| {
                self.state
                    .seconds_at_beat((clip.start_beat + clip.duration_beats) as f64)
                    as f32
                    + 4.0
            })
            .fold(16.0_f32, f32::max);
        let song_text_end_seconds = self
            .state
            .song_text_events
            .last()
            .map(|event| self.state.seconds_at_beat(event.beat) as f32 + 4.0)
            .unwrap_or(0.0);
        let longest_seconds = clip_end_seconds.max(song_text_end_seconds);
        (longest_seconds * self.state.viewport.pixels_per_second).max(1200.0)
    }

    pub fn set_native_audio_callbacks(
        &mut self,
        on_seek_beats: Option<
            std::sync::Arc<dyn Fn(f32, f32, crate::layout::SeekReason) + Send + Sync + 'static>,
        >,
        on_track_param_change: Option<
            std::sync::Arc<dyn Fn(String, String, f32) + Send + Sync + 'static>,
        >,
        on_track_input_state_change: Option<
            std::sync::Arc<
                dyn Fn(String, bool, bool) -> Result<(), String> + Send + Sync + 'static,
            >,
        >,
    ) {
        self.on_seek_beats = on_seek_beats;
        self.on_track_param_change = on_track_param_change;
        self.on_track_input_state_change = on_track_input_state_change;
    }

    pub fn set_playhead_scrub_callbacks(
        &mut self,
        on_begin: Option<
            std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + Send + Sync + 'static>,
        >,
        on_end: Option<
            std::sync::Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + Send + Sync + 'static>,
        >,
    ) {
        self.on_playhead_scrub_begin = on_begin;
        self.on_playhead_scrub_end = on_end;
    }

    pub fn seek_to_beat(&mut self, beat: f32, cx: &mut Context<Self>) {
        self.seek_to_beat_with_reason(beat, crate::layout::SeekReason::TimelineClick, cx);
    }

    pub fn seek_to_beat_with_reason(
        &mut self,
        beat: f32,
        reason: crate::layout::SeekReason,
        cx: &mut Context<Self>,
    ) {
        let snapped_sec = self
            .state
            .snap_time(beat.max(0.0) * self.state.seconds_per_beat());
        let snapped_beat = snapped_sec / self.state.seconds_per_beat();
        self.seek_to_exact_beat(snapped_beat, reason, cx);
    }

    /// Seek to an event's stored musical position without applying the current
    /// edit grid. Used by marker and Song Text navigation so off-grid events
    /// remain seekable exactly.
    pub fn seek_to_exact_beat(
        &mut self,
        beat: f32,
        reason: crate::layout::SeekReason,
        cx: &mut Context<Self>,
    ) {
        self.state.transport.playhead_beats = beat.max(0.0);
        let beat = self.state.transport.playhead_beats;
        self.state.recompute_effective_volumes(beat, "seek");
        if let Some(cb) = self.on_seek_beats.as_ref() {
            cb(beat, self.state.bpm, reason);
        }
        cx.notify();
    }

    pub(super) fn max_scroll_offsets(&self, window: &Window) -> (f32, f32) {
        self.scroll_geometry(window).2
    }

    pub(super) fn scroll_geometry(&self, window: &Window) -> (f32, f32, (f32, f32)) {
        self.scroll_geometry_with_content_height(window, self.state.total_track_rows_height())
    }

    /// Scroll geometry against an already-known arrangement content height.
    ///
    /// The repaint path passes the height from the frame's shared row layout so
    /// the O(track_count) layout build does not run a second time per frame.
    pub(super) fn scroll_geometry_with_content_height(
        &self,
        window: &Window,
        content_h: f32,
    ) -> (f32, f32, (f32, f32)) {
        let window_size = window.bounds().size;
        let window_w: f32 = window_size.width.into();
        let window_h: f32 = window_size.height.into();
        let m = self.chrome_metrics;
        // Width: window minus browser/sidebar (only when actually shown via
        // its measured width), inspector (only when shown), and the
        // timeline's own fixed track-header column.
        let track_view_w = (window_w - m.browser_width - m.inspector_width - HEADER_WIDTH).max(0.0);
        // Height: window minus app chrome, ruler, the actual current
        // bottom panel height (0 when hidden), and status bar. No magic
        // 220 — the previous constant was stale whenever the bottom
        // panel was resized or hidden, which left the timeline either
        // too short (blank bottom area) or too tall (overflowing).
        let used_v = self.state.timeline_origin_y()
            + self.state.arrangement_content_top()
            + m.bottom_panel_height
            + m.status_bar_height;
        let track_view_h = (window_h - used_v).max(DEFAULT_TRACK_HEIGHT);
        let content_w = self.timeline_content_width();

        if std::env::var_os("FUTUREBOARD_TIMELINE_VIEWPORT_DEBUG").is_some() {
            eprintln!(
                "[tl-viewport] window={}x{} body={}x{} browser={} inspector={} bottom={} status={} content={}x{}",
                window_w,
                window_h,
                track_view_w,
                track_view_h,
                m.browser_width,
                m.inspector_width,
                m.bottom_panel_height,
                m.status_bar_height,
                content_w,
                content_h
            );
        }

        (
            track_view_w,
            track_view_h,
            (
                (content_w - track_view_w).max(0.0),
                (content_h - track_view_h).max(0.0),
            ),
        )
    }

    pub(super) fn move_dragged_clip_to_position(
        &mut self,
        drag: &ClipDragItem,
        position: gpui::Point<gpui::Pixels>,
        window: &Window,
    ) {
        self.move_dragged_clip_to_position_with_bypass(drag, position, window, false)
    }

    pub(super) fn move_dragged_clip_to_position_with_bypass(
        &mut self,
        drag: &ClipDragItem,
        position: gpui::Point<gpui::Pixels>,
        window: &Window,
        bypass_snap: bool,
    ) {
        let origin = *self.clip_drag_origin.get_or_insert(position);
        if self
            .clip_move_origin
            .as_ref()
            .is_none_or(|move_origin| move_origin.anchor_clip_id != drag.clip_id)
        {
            // The first move of this drag: nothing has changed yet, so this is
            // the "before" of the one undo step the drop records — every clip
            // that moves with the grabbed one, at its index in its track.
            let clips: Vec<ClipSnapshot> = self
                .clip_drag_selection_ids(&drag.clip_id)
                .iter()
                .filter_map(|id| ClipSnapshot::capture(&self.state, id))
                .collect();
            let Some(anchor_start) = clips
                .iter()
                .find(|snapshot| snapshot.clip.id == drag.clip_id)
                .map(|snapshot| snapshot.clip.start_beat)
            else {
                return;
            };
            self.clip_move_origin = Some(ClipMoveOrigin {
                anchor_clip_id: drag.clip_id.clone(),
                anchor_start,
                clips,
            });
            // A drag, not a click: the press's deferred selection change is off.
            self.pending_clip_click = None;
        }
        let (target_index, snapped) =
            self.resolve_clip_drag_target_with_bypass(drag, origin, position, bypass_snap);
        self.clip_drag_target_track_index = Some(target_index);

        let Some(move_origin) = self.clip_move_origin.as_ref() else {
            return;
        };
        // Every clip from its origin plus one anchor delta: the preview never
        // drifts, and the drop commits exactly these positions.
        let origin_starts: Vec<f32> = move_origin
            .clips
            .iter()
            .map(|snapshot| snapshot.clip.start_beat)
            .collect();
        let starts = crate::components::timeline::timeline_state::group_move_starts(
            &origin_starts,
            move_origin.anchor_start,
            snapped,
        );
        let moves: Vec<(String, f32)> = move_origin
            .clips
            .iter()
            .map(|snapshot| snapshot.clip.id.clone())
            .zip(starts)
            .collect();
        for (clip_id, start) in moves {
            // In place: the preview must not reorder the track's clips, or the
            // captured indices would no longer say where each clip was.
            self.state.set_clip_start_in_place(&clip_id, start);
        }

        let (max_x, max_y) = self.max_scroll_offsets(window);
        self.state.viewport.scroll_x = self.state.viewport.scroll_x.clamp(0.0, max_x);
        self.state.viewport.scroll_y = self.state.viewport.scroll_y.clamp(0.0, max_y);
    }

    /// Commit the clip move in flight as one `UpdateClips` undo step.
    ///
    /// The drop passes the track the pointer is over, and the group moves
    /// across tracks by that many rows, exactly at the positions the preview
    /// showed (nothing is snapped again). A release outside the timeline gets
    /// no drop, so it commits with `None`: every clip stays on its own track,
    /// where the lanes are already showing it — without this the moved clips
    /// stayed moved with no undo entry at all.
    pub(super) fn commit_clip_move(
        &mut self,
        anchor_clip_id: &str,
        target_track_index: Option<usize>,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(origin) = self
            .clip_move_origin
            .take()
            .filter(|origin| origin.anchor_clip_id == anchor_clip_id)
        else {
            return;
        };
        let clip_ids: Vec<String> = origin
            .clips
            .iter()
            .map(|snapshot| snapshot.clip.id.clone())
            .collect();
        let mut target_track_id = None;
        if let Some(target_index) = target_track_index {
            let source_index = self
                .state
                .tracks
                .iter()
                .position(|track| {
                    origin
                        .clips
                        .iter()
                        .any(|s| s.clip.id == anchor_clip_id && s.track_id == track.id)
                })
                .unwrap_or(target_index);
            let track_delta = target_index as isize - source_index as isize;
            let max_index = self.state.tracks.len().saturating_sub(1) as isize;
            for clip_id in &clip_ids {
                let Some((track_index, start)) =
                    self.state
                        .tracks
                        .iter()
                        .enumerate()
                        .find_map(|(index, track)| {
                            track
                                .clips
                                .iter()
                                .find(|clip| clip.id == *clip_id)
                                .map(|clip| (index, clip.start_beat))
                        })
                else {
                    continue;
                };
                let Some(track_id) = self
                    .state
                    .tracks
                    .get((track_index as isize + track_delta).clamp(0, max_index) as usize)
                    .map(|track| track.id.clone())
                else {
                    continue;
                };
                self.state
                    .move_clip_to_track_unsnapped(clip_id, &track_id, start);
            }
            target_track_id = self
                .state
                .tracks
                .get(target_index)
                .map(|track| track.id.clone());
        }
        let next: Vec<ClipSnapshot> = clip_ids
            .iter()
            .filter_map(|id| ClipSnapshot::capture(&self.state, id))
            .collect();
        let previous: Vec<ClipSnapshot> = origin
            .clips
            .into_iter()
            .filter(|before| next.iter().any(|after| after.clip.id == before.clip.id))
            .collect();
        self.restore_clip_drag_selection(anchor_clip_id, clip_ids, target_track_id);
        let changed = previous.iter().zip(&next).any(|(before, after)| {
            before.track_id != after.track_id
                || before.index != after.index
                || before.clip != after.clip
        });
        if changed {
            self.record_executed_command(EditCommand::UpdateClips { previous, next }, cx);
        }
    }

    /// The release of a press on a clip that did not become a drag: apply the
    /// selection change the press deferred. Skipped when the clip is gone —
    /// an Option-click in the cut zone splits it before this runs.
    pub(super) fn apply_pending_clip_click(&mut self) -> bool {
        use crate::components::timeline::timeline_state::ClipSelectOp;
        let Some(pending) = self.pending_clip_click.take() else {
            return false;
        };
        if self.clip_move_origin.is_some() || self.state.find_clip(&pending.clip_id).is_none() {
            return false;
        }
        match pending.op {
            ClipSelectOp::Replace => self.state.select_clip(&pending.clip_id),
            ClipSelectOp::Toggle => self.state.select_clip_additive(&pending.clip_id),
        }
        true
    }

    pub(super) fn clip_drag_selection_ids(&self, dragged_clip_id: &str) -> Vec<String> {
        let selected = &self.state.selection.selected_clip_ids;
        if selected.iter().any(|id| id == dragged_clip_id) {
            selected
                .iter()
                .filter(|id| self.state.find_clip(id).is_some())
                .cloned()
                .collect()
        } else {
            vec![dragged_clip_id.to_string()]
        }
    }

    pub(super) fn restore_clip_drag_selection(
        &mut self,
        dragged_clip_id: &str,
        clip_ids: Vec<String>,
        fallback_track_id: Option<String>,
    ) {
        let existing = clip_ids
            .into_iter()
            .filter(|id| self.state.find_clip(id).is_some())
            .collect::<Vec<_>>();
        if existing.is_empty() {
            return;
        }

        let selected_track_id = self
            .state
            .find_clip(dragged_clip_id)
            .map(|(track, _)| track.id.clone())
            .or(fallback_track_id)
            .or_else(|| {
                existing
                    .first()
                    .and_then(|id| self.state.find_clip(id))
                    .map(|(track, _)| track.id.clone())
            });
        self.state.selection.selected_track_id = selected_track_id;
        self.state.selection.selected_clip_ids = existing;
    }

    pub(super) fn resolve_clip_drag_target(
        &self,
        drag: &ClipDragItem,
        origin: gpui::Point<gpui::Pixels>,
        position: gpui::Point<gpui::Pixels>,
    ) -> (usize, f32) {
        self.resolve_clip_drag_target_with_bypass(drag, origin, position, false)
    }

    pub(super) fn resolve_clip_drag_target_with_bypass(
        &self,
        drag: &ClipDragItem,
        origin: gpui::Point<gpui::Pixels>,
        position: gpui::Point<gpui::Pixels>,
        bypass_snap: bool,
    ) -> (usize, f32) {
        // Through the transform, not `dx / ppb`: under tempo automation a
        // pixel is a different number of beats in different bars.
        let dx: f32 = (position.x - origin.x).into();
        let start_x = self.state.beats_to_x(drag.start_beat);
        let new_start = self.state.x_to_beats(start_x + dx).max(0.0);
        let snapped = self
            .state
            .snap_beats_with_bypass(new_start, bypass_snap)
            .max(0.0);

        let source_index = self
            .state
            .tracks
            .iter()
            .position(|track| track.id == drag.source_track_id)
            .unwrap_or(0);
        let viewport_y = self.track_area_y_from_window(position);
        let target_index = self
            .state
            .track_index_at_y(viewport_y)
            .unwrap_or(source_index);
        (target_index, snapped)
    }

    pub(super) fn build_clip_clone_at(
        &self,
        source_clip_id: &str,
        target_track_id: &str,
        start_beat: f32,
    ) -> Option<(String, ClipState)> {
        let (_, source) = self.state.find_clip(source_clip_id)?;
        let clip = self.state.clone_clip_for_insert(
            source,
            self.state.next_clip_id(),
            format!("{} Copy", source.name),
            start_beat,
        );
        Some((target_track_id.to_string(), clip))
    }

    pub(super) fn create_clip_clone_at(
        &mut self,
        source_clip_id: &str,
        target_track_id: &str,
        start_beat: f32,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let Some((track_id, clip)) =
            self.build_clip_clone_at(source_clip_id, target_track_id, start_beat)
        else {
            return false;
        };
        self.run_edit_command(EditCommand::CreateClip { track_id, clip }, cx);
        true
    }

    pub(super) fn create_clip_clone_group_at(
        &mut self,
        anchor_clip_id: &str,
        target_track_id: &str,
        anchor_start_beat: f32,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        let selected = self.clip_drag_selection_ids(anchor_clip_id);
        if selected.len() <= 1 {
            return self.create_clip_clone_at(
                anchor_clip_id,
                target_track_id,
                anchor_start_beat,
                cx,
            );
        }

        let Some((anchor_track, anchor_clip)) = self.state.find_clip(anchor_clip_id) else {
            return false;
        };
        let Some(source_track_index) = self
            .state
            .tracks
            .iter()
            .position(|track| track.id == anchor_track.id)
        else {
            return false;
        };
        let Some(target_track_index) = self
            .state
            .tracks
            .iter()
            .position(|track| track.id == target_track_id)
        else {
            return false;
        };

        let beat_delta = anchor_start_beat - anchor_clip.start_beat;
        let track_delta = target_track_index as isize - source_track_index as isize;
        let max_index = self.state.tracks.len().saturating_sub(1) as isize;

        let mut used_ids: std::collections::HashSet<String> = self
            .state
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter().map(|clip| clip.id.clone()))
            .collect();
        let mut next_clip_id = || {
            let mut n = 1u32;
            loop {
                let id = format!("clip-{n}");
                if used_ids.insert(id.clone()) {
                    return id;
                }
                n = n.saturating_add(1);
            }
        };

        let mut clips = Vec::new();
        for clip_id in selected {
            let Some((source_track, source_clip)) = self.state.find_clip(&clip_id) else {
                continue;
            };
            let Some(source_index) = self
                .state
                .tracks
                .iter()
                .position(|track| track.id == source_track.id)
            else {
                continue;
            };
            let target_index = (source_index as isize + track_delta).clamp(0, max_index) as usize;
            let Some(track) = self.state.tracks.get(target_index) else {
                continue;
            };
            let start = (source_clip.start_beat + beat_delta).max(0.0);
            let clip = self.state.clone_clip_for_insert(
                source_clip,
                next_clip_id(),
                format!("{} Copy", source_clip.name),
                start,
            );
            clips.push((track.id.clone(), clip));
        }

        if clips.is_empty() {
            return false;
        }
        self.run_edit_command(EditCommand::BatchCreateClips { clips }, cx);
        true
    }

    pub(super) fn track_area_y_from_window(&self, position: gpui::Point<gpui::Pixels>) -> f32 {
        let y: f32 = position.y.into();
        self.state.track_viewport_y_from_window_y(y).max(0.0)
    }

    pub(super) fn import_audio_path_at_last_drag(
        &mut self,
        path: &std::path::Path,
        force_new_track: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !is_supported_audio_ext(path) {
            return false;
        }

        let path_key = path.to_string_lossy().to_string();
        let clip_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Imported Audio".to_string());

        let (drop_x, drop_y) = self.drop_position_or_new_track(force_new_track);

        let clip_id = self
            .state
            .import_audio_at(path_key.clone(), clip_name, drop_x, drop_y);
        self.mark_project_changed(cx);
        self.mark_media_changed(cx);
        super::super::audio_import::spawn_timeline_import(
            path.to_path_buf(),
            self.project_root.clone(),
            vec![clip_id],
            cx.entity().clone(),
            None,
            cx,
        );
        true
    }

    /// Places a dropped video on the Video track, creating the track if the
    /// project has none.
    ///
    /// Unlike audio and MIDI this ignores `force_new_track`: the Video track is
    /// a singleton, so a multi-file drop stacks reference videos on the one
    /// track rather than minting a second one.
    pub(super) fn import_video_path_at_last_drag(
        &mut self,
        path: &std::path::Path,
        _force_new_track: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !sphere_video_player::is_supported_video_path(path) {
            return false;
        }

        let path_key = path.to_string_lossy().to_string();
        let clip_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Reference Video".to_string());

        let (drop_x, _drop_y) = self.drop_position_or_new_track(false);
        let clip_id = self.state.import_video_at(path_key, clip_name, drop_x);
        self.mark_project_changed(cx);
        self.mark_media_changed(cx);
        self.spawn_video_duration_probe(path.to_path_buf(), clip_id, cx);
        true
    }

    /// Replaces an imported video clip's placeholder length with the container's
    /// real duration.
    ///
    /// Opening a decoder blocks (it parses the container and may seek), so the
    /// probe runs on the background executor and only the resulting number
    /// crosses back to the UI. A file the platform cannot decode simply leaves
    /// the placeholder length in place — the clip is still movable and the
    /// player window reports the real error.
    fn spawn_video_duration_probe(
        &mut self,
        path: std::path::PathBuf,
        clip_id: String,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let probe_path = path.clone();
            let probed = cx
                .background_executor()
                .spawn(async move { sphere_video_player::probe(&probe_path) })
                .await;
            let seconds = match probed {
                Ok(info) if info.duration_seconds > 0.0 => info.duration_seconds,
                Ok(_) => return,
                Err(error) => {
                    eprintln!(
                        "[video-import] probe failed path={} err={error}",
                        path.display()
                    );
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                if this
                    .state
                    .set_video_clip_duration_seconds(&clip_id, seconds)
                {
                    this.mark_project_changed(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn import_midi_path_at_last_drag(
        &mut self,
        path: &std::path::Path,
        force_new_track: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !is_supported_midi_ext(path) {
            return false;
        }
        let Some(imported_tracks) = read_and_parse_midi_file(path) else {
            return false;
        };
        let (drop_x, drop_y) = self.drop_position_or_new_track(force_new_track);

        // A file that carries markers, controller lanes, or SysEx goes through
        // the import dialog first — a song file can ship hundreds of markers,
        // and dropping them all onto the ruler unasked is what made this
        // unusable. A plain note-only file has nothing to ask about, so it
        // still lands on the drop.
        let summary = super::super::midi_import::MidiImportSummary::of(&imported_tracks);
        if summary.has_optional_payload() {
            if let Some(prompt) = self.on_midi_import_prompt.clone() {
                let request = TimelineMidiImportPrompt {
                    path: path.to_path_buf(),
                    file_name: midi_import_display_name(path),
                    summary,
                    drop_x,
                    drop_y,
                };
                prompt(&request, window, cx);
                return true;
            }
        }

        self.apply_midi_import(
            path,
            imported_tracks,
            super::super::midi_import::MidiImportOptions::default(),
            drop_x,
            drop_y,
            cx,
        )
    }

    /// Import a MIDI file with the options the import dialog returned, at the
    /// lane coordinates its drop resolved to. Re-reads the file so nothing has
    /// to be parked across the dialog.
    pub fn import_midi_path_with_options(
        &mut self,
        path: &std::path::Path,
        options: super::super::midi_import::MidiImportOptions,
        drop_x: f32,
        drop_y: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(imported_tracks) = read_and_parse_midi_file(path) else {
            return false;
        };
        self.apply_midi_import(path, imported_tracks, options, drop_x, drop_y, cx)
    }

    fn apply_midi_import(
        &mut self,
        path: &std::path::Path,
        mut imported_tracks: Vec<super::super::midi_import::ImportedMidiTrack>,
        options: super::super::midi_import::MidiImportOptions,
        drop_x: f32,
        drop_y: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        super::super::midi_import::apply_import_options(&mut imported_tracks, options);
        let clip_name = midi_import_display_name(path);
        let clips = self
            .state
            .import_midi_tracks_at(clip_name, imported_tracks, drop_x, drop_y);
        if clips.is_empty() {
            return false;
        }
        if crate::components::timeline::timeline_state::midi_debug_enabled() {
            let note_count: usize = clips
                .iter()
                .map(|(_, clip)| match &clip.clip_type {
                    crate::components::timeline::timeline_state::ClipType::Midi {
                        notes, ..
                    } => notes.len(),
                    _ => 0,
                })
                .sum();
            eprintln!(
                "[MidiImport] imported path={} clips={} notes={}",
                path.display(),
                clips.len(),
                note_count,
            );
        }
        if clips.len() == 1 {
            let (track_id, clip) = clips.into_iter().next().expect("single imported clip");
            self.run_edit_command(EditCommand::CreateClip { track_id, clip }, cx);
        } else {
            self.run_edit_command(EditCommand::BatchCreateClips { clips }, cx);
        }
        true
    }

    pub(super) fn drop_plugin_preset_at_last_drag(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pst"))
        {
            return false;
        }
        let Some(position) = self.last_drag_position else {
            return false;
        };
        let target = self.resolve_context_target_from_window_point(position);
        let track_id = match target {
            TimelineContextTarget::TrackLane { track_id, .. }
            | TimelineContextTarget::AudioClip { track_id, .. }
            | TimelineContextTarget::MidiClip { track_id, .. }
            | TimelineContextTarget::TrackHeader(track_id) => Some(track_id),
            // Empty space below the last track: the preset gets a new track
            // of its own, the same as an audio file dropped there.
            TimelineContextTarget::TimelineEmpty => None,
            _ => return false,
        };
        let Some(callback) = self.on_plugin_preset_drop.as_ref() else {
            return false;
        };
        callback(&(path.to_path_buf(), track_id), window, cx);
        true
    }

    pub(super) fn drop_position_or_new_track(&self, force_new_track: bool) -> (f32, f32) {
        match self.last_drag_position {
            Some(p) if !force_new_track => {
                let x: f32 = p.x.into();
                let y: f32 = p.y.into();
                let lane_x = self.state.lane_x_from_window_x(x).max(0.0);
                let lane_y = self.state.track_viewport_y_from_window_y(y).max(0.0);
                (lane_x, lane_y)
            }
            _ => (0.0, 1.0e9_f32),
        }
    }
}

/// Read and parse a Standard MIDI File, reporting the failing stage. `None`
/// means nothing was imported — the caller falls through to the next handler.
fn read_and_parse_midi_file(
    path: &std::path::Path,
) -> Option<Vec<crate::components::timeline::midi_import::ImportedMidiTrack>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!(
                "[MidiImport] read failed path={} err={error}",
                path.display()
            );
            return None;
        }
    };
    match crate::components::timeline::midi_import::parse_smf_tracks(&bytes) {
        Ok(imported) => Some(imported),
        Err(error) => {
            eprintln!(
                "[MidiImport] parse failed path={} err={error}",
                path.display()
            );
            None
        }
    }
}

/// File stem used both as the imported clip's base name and as the dialog's
/// subject line.
fn midi_import_display_name(path: &std::path::Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .unwrap_or_else(|| "Imported MIDI".to_string())
}

use gpui::{Context, Window};

use std::sync::Arc;

use crate::components;
use crate::components::numeric_edit::NumericEditSession;
use crate::components::text_input::{
    bind_mouse_selection, TextInputCallbacks, TextInputMouseEvent, TextInputMousePhase,
    TextInputState,
};
use crate::components::{PerformanceOverlaySnapshot, StatusBarContent, StatusBarPerfMetrics};

use super::{
    ContextMenuRequest, ContextMenuTarget, ContextTarget, RecordingUiState, StudioLayout,
    TransportCommand,
};

/// Undo label shared by every tap in one tap-tempo session — the key the
/// history uses to recognise the entry it should extend.
const TAP_TEMPO_EDIT_LABEL: &str = "Tap Tempo";

fn tap_tempo_now_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

/// Inline BPM + time-signature numeric editors opened from the transport bar
/// (the small pop-up fields on the BPM / time-sig displays). Second
/// `StudioLayout` decomposition slice — accessed across the transport ops
/// modules. Holds focus-handle-backed text inputs, so it is built via
/// `new(cx)` rather than `Default`.
pub(crate) struct TempoEditState {
    /// Inline numeric BPM editor field.
    pub bpm_input: TextInputState,
    /// Whether the inline BPM editor is open.
    pub bpm_editing: bool,
    /// Shared numeric edit session for the inline BPM editor. Owns the value
    /// range/format/commit policy so typed edits and drag adjustments resolve
    /// through one value source; the live draft stays in `bpm_input.value`.
    pub bpm_session: Option<NumericEditSession>,
    /// Inline time-signature numerator field.
    pub ts_num_input: TextInputState,
    /// Inline time-signature denominator field.
    pub ts_den_input: TextInputState,
    /// Whether the inline time-signature editor is open.
    pub ts_editing: bool,
    /// `Some(id)` edits one time-sig marker; `None` edits the project default.
    pub ts_edit_point_id: Option<String>,
    /// True while the numerator field holds focus (false → denominator).
    pub ts_edit_focus_num: bool,
}

impl TempoEditState {
    pub(super) fn new(cx: &mut Context<StudioLayout>) -> Self {
        Self {
            bpm_input: TextInputState::new("transport-bpm-input", cx.focus_handle())
                .with_accessible_label("Tempo in BPM"),
            bpm_editing: false,
            bpm_session: None,
            ts_num_input: TextInputState::new("transport-ts-num-input", cx.focus_handle())
                .with_accessible_label("Time signature numerator"),
            ts_den_input: TextInputState::new("transport-ts-den-input", cx.focus_handle())
                .with_accessible_label("Time signature denominator"),
            ts_editing: false,
            ts_edit_point_id: None,
            ts_edit_focus_num: true,
        }
    }
}

/// Right-click handler that opens the shared text context menu for one of the
/// transport readout's inline editors.
fn transport_text_context(
    target: gpui::Entity<StudioLayout>,
    menu_target: crate::layout::studio_state::TextMenuTarget,
) -> crate::components::text_input::TextInputContextCb {
    Arc::new(move |pos: &(f32, f32), _window, cx| {
        let (x, y) = *pos;
        let _ = target.update(cx, |layout, cx| {
            layout.menu_bar.open_menu_id = None;
            layout.menu_bar.submenu_path.clear();
            layout.overlay.text_context_menu = Some(crate::layout::studio_state::TextContextMenu {
                target: menu_target,
                x,
                y,
            });
            cx.notify();
        });
    })
}

fn bind_time_signature_mouse_selection(
    target: gpui::Entity<StudioLayout>,
    numerator: bool,
) -> TextInputCallbacks {
    TextInputCallbacks {
        on_context_command: None,
        on_context_menu: Some(transport_text_context(
            target.clone(),
            if numerator {
                crate::layout::studio_state::TextMenuTarget::TransportTimeSigNum
            } else {
                crate::layout::studio_state::TextMenuTarget::TransportTimeSigDen
            },
        )),
        on_mouse: Some(Arc::new(move |event: &TextInputMouseEvent, _window, cx| {
            let _ = target.update(cx, |layout, cx| {
                if matches!(event.phase, TextInputMousePhase::Down) {
                    layout.tempo_edit.ts_edit_focus_num = numerator;
                }
                let input = if numerator {
                    &mut layout.tempo_edit.ts_num_input
                } else {
                    &mut layout.tempo_edit.ts_den_input
                };
                match event.phase {
                    TextInputMousePhase::Down => input.handle_mouse_down(event.index, event.extend),
                    TextInputMousePhase::Drag => input.handle_mouse_drag(event.index),
                    TextInputMousePhase::Up => input.handle_mouse_up(),
                }
                cx.notify();
            });
        })),
    }
}

impl StudioLayout {
    pub(super) fn zoom_timeline_by(&self, cx: &mut Context<Self>, factor: f32) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.state.zoom_by(factor, 0.0);
            cx.notify();
        });
    }

    pub(super) fn reset_timeline_zoom(&self, cx: &mut Context<Self>) {
        let _ = self.timeline.update(cx, |timeline, cx| {
            let current = timeline.state.viewport.pixels_per_second.max(0.0001);
            // 150 px/s matches the Web UI default zoom (see timeline_state.rs:460).
            let factor = 150.0 / current;
            timeline.state.zoom_by(factor, 0.0);
            cx.notify();
        });
    }

    pub(super) fn project_end_beat(&self, cx: &mut Context<Self>) -> f32 {
        let timeline = self.timeline.read(cx);
        timeline
            .state
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .map(|clip| clip.start_beat + clip.duration_beats)
            .fold(0.0_f32, f32::max)
    }

    pub(super) fn nudge_playhead_bars(&mut self, cx: &mut Context<Self>, bars: f32) {
        let (current_beat, num) = {
            let timeline = self.timeline.read(cx);
            (
                timeline.state.transport.playhead_beats,
                timeline.state.time_signature_num as f32,
            )
        };
        let target = (current_beat + bars * num.max(1.0)).max(0.0);
        self.seek_native_playhead(cx, target);
    }

    pub(super) fn dispatch_transport_command(
        &mut self,
        command: TransportCommand,
        cx: &mut Context<Self>,
    ) {
        if !self.session_install_status.is_ready() {
            eprintln!("[SessionLoad] transport blocked during session install");
            return;
        }
        match command {
            TransportCommand::PlayPause => {
                if self.is_recording_active(cx) {
                    self.log_transport_debug("Spacebar", "stop_recording_and_stop_transport", cx);
                    self.stop_native_playback(cx);
                    return;
                }
                // Space starts the take when a track is armed and the transport
                // is parked — count-in and all. Arming a track is the statement
                // that the next roll is a take, and having to reach for a second
                // control to say it again is how a first pass gets lost.
                //
                // Only from a standstill: space while playing is Stop, which is
                // the one thing about a transport nobody should have to think
                // about. Nothing armed and it plays, as it always did.
                let stopped = !self
                    .audio_bridge
                    .stats
                    .as_ref()
                    .map(|stats| stats.transport_playing)
                    .unwrap_or(false);
                if stopped && self.any_track_record_armed(cx) {
                    self.log_transport_debug("Spacebar", "start_recording_armed", cx);
                    self.start_native_recording(cx);
                    return;
                }
                let playing = self
                    .audio_bridge
                    .stats
                    .as_ref()
                    .map(|stats| stats.transport_playing)
                    .unwrap_or(false);
                if playing {
                    self.stop_native_playback(cx);
                } else {
                    self.start_native_playback(cx);
                }
            }
            TransportCommand::Stop => {
                // Same call as Spacebar below: `stop_native_playback` is the one
                // Stop, and it decides whether a take has to be finalized first.
                if self.is_recording_active(cx) {
                    self.log_transport_debug("Stop", "stop_recording_and_stop_transport", cx);
                }
                self.stop_native_playback(cx);
            }
            TransportCommand::ReturnToStart => self.seek_native_playhead(cx, 0.0),
            TransportCommand::ToggleLoop => {
                let _ = self.timeline.update(cx, |timeline, cx| {
                    let enabling = !timeline.state.transport.loop_enabled;
                    if enabling {
                        if let Some(range) = timeline.state.arrangement_range.clone() {
                            let (start, end) = range.as_f32_range();
                            if end > start {
                                timeline.state.transport.loop_start_beats = start;
                                timeline.state.transport.loop_end_beats = end;
                            }
                        } else {
                            let still_default = (timeline.state.transport.loop_start_beats - 0.0)
                                .abs()
                                < f32::EPSILON
                                && (timeline.state.transport.loop_end_beats - 16.0).abs()
                                    < f32::EPSILON;
                            if still_default {
                                // Still on the empty-project default — seed one bar
                                // around the playhead so enabling Loop is visible.
                                let bar = timeline
                                    .state
                                    .time_signature_map
                                    .points
                                    .first()
                                    .map(|p| p.numerator.max(1) as f32)
                                    .unwrap_or(4.0);
                                let playhead = timeline.state.transport.playhead_beats.max(0.0);
                                timeline.state.transport.loop_start_beats = playhead;
                                timeline.state.transport.loop_end_beats = playhead + bar;
                            }
                        }
                    }
                    timeline.state.transport.loop_enabled = enabling;
                    cx.notify();
                });
                self.sync_loop_controls(cx);
            }
            TransportCommand::ToggleMetronome => {
                let enabled = self.timeline.update(cx, |timeline, cx| {
                    timeline.state.transport.metronome_enabled =
                        !timeline.state.transport.metronome_enabled;
                    let enabled = timeline.state.transport.metronome_enabled;
                    cx.notify();
                    enabled
                });
                // The toolbar button and the Settings checkbox are the same
                // switch. Persist it here or the next settings change of any
                // kind re-reads the schema, finds the stale value, and turns the
                // click back off — button, engine and all. Written straight
                // through `SettingsModel` rather than `handle_setting_update` so
                // a transport command does not re-enter `sync_settings_to_systems`
                // (which is what would push the reverted value back).
                self.settings.update(cx, |settings, cx| {
                    settings.update_setting(
                        move |schema| schema.recording.metronome.enabled = enabled,
                        cx,
                    );
                });
                if let (enabled, Some(engine)) = (enabled, self.audio_bridge.engine.as_ref()) {
                    if let Err(error) = engine.set_metronome_enabled(enabled) {
                        if !matches!(error, DirectAudio::SphereAudioError::EngineNotOpen) {
                            eprintln!("[audio] set metronome failed: {error}");
                        }
                    }
                }
            }
            TransportCommand::ToggleFollowPlayhead => {
                let enabled = self.timeline.update(cx, |timeline, cx| {
                    timeline.state.follow_playhead = !timeline.state.follow_playhead;
                    let enabled = timeline.state.follow_playhead;
                    cx.notify();
                    enabled
                });
                if std::env::var_os("FUTUREBOARD_AUTOSCROLL_DEBUG").is_some() {
                    eprintln!("[autoscroll] toggled follow_playhead -> {}", enabled);
                }
            }
            TransportCommand::ToggleAutoScrollMode => {
                let mode = self.timeline.update(cx, |timeline, cx| {
                    let mode = timeline.state.toggle_auto_scroll_mode();
                    // Switching the mode is a request to auto-scroll that way, so
                    // make sure following is on — otherwise nothing visibly changes.
                    timeline.state.set_follow_playhead(true);
                    cx.notify();
                    mode
                });
                if std::env::var_os("FUTUREBOARD_AUTOSCROLL_DEBUG").is_some() {
                    eprintln!("[autoscroll] toggled auto_scroll_mode -> {:?}", mode);
                }
            }
            TransportCommand::Record => {
                if self.is_recording_active(cx) {
                    self.log_transport_debug("Record", "stop_recording_and_stop_transport", cx);
                }
                self.toggle_native_recording(cx)
            }
        }
    }

    pub(super) fn is_recording_active(&self, cx: &mut Context<Self>) -> bool {
        matches!(
            self.recording.ui_state,
            RecordingUiState::Preparing
                | RecordingUiState::CountingIn { .. }
                | RecordingUiState::Recording
                | RecordingUiState::Finalizing
        ) || self.timeline.read(cx).state.transport.recording
            || !self.recording.preview.is_empty()
            || self.recording.midi.is_some()
            || self
                .audio_bridge
                .engine
                .as_ref()
                .map(|engine| engine.recording_status().active)
                .unwrap_or(false)
    }

    pub(super) fn log_transport_debug(&self, event: &str, action: &str, cx: &mut Context<Self>) {
        if std::env::var_os("FUTUREBOARD_TRANSPORT_DEBUG").is_none() {
            return;
        }
        let timeline = self.timeline.read(cx);
        eprintln!(
            "[TransportDebug] event={} before playing={} recording={} active_recording_id={:?} action={}",
            event,
            timeline.state.transport.playing,
            timeline.state.transport.recording,
            self.recording.preview.values().next().map(|p| p.recording_id),
            action
        );
    }

    pub(super) fn transport_chrome_state(
        &self,
        cx: &mut Context<Self>,
    ) -> components::TransportChromeState {
        let (
            position_label,
            bpm_value,
            bpm_label,
            bpm_has_automation,
            time_signature_label,
            ts_has_markers,
            recording,
            loop_enabled,
            metronome_enabled,
            follow_playhead,
            auto_scroll_continuous,
            count_in_enabled,
        ) = {
            let timeline = self.timeline.read(cx);
            // The transport always shows the *effective* BPM at the playhead so
            // tempo automation is visible without opening the Tempo Track.
            let bpm = timeline.state.effective_bpm_at_playhead() as f32;
            let bpm_has_automation = timeline.state.tempo_has_automation();
            let bpm_label = if (bpm.fract()).abs() < 0.05 {
                format!("{:.0}", bpm)
            } else {
                format!("{:.1}", bpm)
            };
            (
                timeline
                    .state
                    .format_position(timeline.state.transport.playhead_beats),
                bpm,
                bpm_label,
                bpm_has_automation,
                {
                    let pt = timeline.state.time_signature_at_playhead();
                    format!("{}/{}", pt.numerator, pt.denominator)
                },
                timeline.state.time_signature_has_markers(),
                timeline.state.transport.recording
                    || self
                        .audio_bridge
                        .engine
                        .as_ref()
                        .map(|engine| engine.recording_status().active)
                        .unwrap_or(false),
                timeline.state.transport.loop_enabled,
                timeline.state.transport.metronome_enabled,
                timeline.state.follow_playhead,
                timeline.state.auto_scroll_mode
                    == components::timeline::timeline_state::AutoScrollMode::Continuous,
                self.settings
                    .read(cx)
                    .current
                    .recording
                    .metronome
                    .count_in_enabled,
            )
        };
        let playing = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|stats| stats.transport_playing)
            .unwrap_or(false);
        let make_command_handler = |command_id: &'static str| {
            let this = cx.entity().clone();
            Arc::new(move |_: &(), _window: &mut Window, cx: &mut gpui::App| {
                let _ = this.update(cx, |this, cx| {
                    this.dispatch_command_id(command_id, cx);
                    cx.notify();
                });
            })
        };

        let on_return_to_start = make_command_handler("transport:go-to-start");
        let on_play_toggle = make_command_handler("transport:play-pause");
        let on_stop = make_command_handler("transport:stop");
        let on_loop_toggle = make_command_handler("transport:toggle-loop");
        let on_metronome_toggle = make_command_handler("transport:toggle-metronome");
        let on_follow_toggle = make_command_handler("transport:toggle-follow-playhead");
        let on_follow_mode_toggle = make_command_handler("transport:toggle-autoscroll-mode");
        let on_record = make_command_handler("transport:record");
        let on_count_in_toggle: components::ChromeActionCb = {
            let this = cx.entity().clone();
            Arc::new(move |_: &(), _window: &mut Window, cx: &mut gpui::App| {
                let _ = this.update(cx, |this, cx| {
                    let current = this
                        .settings
                        .read(cx)
                        .current
                        .recording
                        .metronome
                        .count_in_enabled;
                    this.settings.update(cx, |settings, cx| {
                        settings.update_setting(
                            move |schema| schema.recording.metronome.count_in_enabled = !current,
                            cx,
                        );
                    });
                    cx.notify();
                });
            })
        };
        let on_count_in_menu: components::BpmMenuCb = {
            let this = cx.entity().clone();
            Arc::new(
                move |pos: &(f32, f32), window: &mut Window, cx: &mut gpui::App| {
                    let (x, y) = *pos;
                    let _ = this.update(cx, |this, cx| {
                        this.try_open_context_menu(
                            ContextMenuRequest::from_window(
                                window,
                                x,
                                y,
                                ContextMenuTarget::Extended(ContextTarget::CountIn),
                            ),
                            cx,
                        );
                    });
                },
            )
        };

        let on_set_bpm: components::BpmChangeCb = {
            let this = cx.entity().clone();
            Arc::new(move |bpm: &f32, _window: &mut Window, cx: &mut gpui::App| {
                let bpm = bpm.clamp(components::BPM_MIN, components::BPM_MAX);
                let _ = this.update(cx, |this, cx| {
                    this.set_native_bpm(bpm, cx);
                });
            })
        };

        let on_bpm_drag: components::BpmDragCb = {
            let this = cx.entity().clone();
            Arc::new(
                move |sample: &components::BpmDragSample,
                      _window: &mut Window,
                      cx: &mut gpui::App| {
                    let sample = *sample;
                    let _ = this.update(cx, |this, cx| {
                        this.apply_bpm_drag_sample(sample, cx);
                    });
                },
            )
        };

        let on_bpm_drag_end: components::ChromeActionCb = {
            let this = cx.entity().clone();
            Arc::new(move |_: &(), _window: &mut Window, cx: &mut gpui::App| {
                let _ = this.update(cx, |this, cx| {
                    this.commit_bpm_drag(cx);
                });
            })
        };

        let on_bpm_menu: components::BpmMenuCb = {
            let this = cx.entity().clone();
            Arc::new(
                move |pos: &(f32, f32), window: &mut Window, cx: &mut gpui::App| {
                    let (x, y) = *pos;
                    let _ = this.update(cx, |this, cx| {
                        this.open_tempo_menu(window, x, y, cx);
                    });
                },
            )
        };

        let on_bpm_edit_start: components::ChromeActionCb = {
            let this = cx.entity().clone();
            Arc::new(move |_: &(), _window: &mut Window, cx: &mut gpui::App| {
                let _ = this.update(cx, |this, cx| {
                    this.begin_bpm_edit(cx);
                });
            })
        };

        let on_tap_tempo: components::ChromeActionCb = {
            let this = cx.entity().clone();
            Arc::new(move |_: &(), _window: &mut Window, cx: &mut gpui::App| {
                let _ = this.update(cx, |this, cx| {
                    this.tap_tempo_now(cx);
                });
            })
        };

        let on_tap_tempo_menu: components::BpmMenuCb = {
            let this = cx.entity().clone();
            Arc::new(
                move |pos: &(f32, f32), window: &mut Window, cx: &mut gpui::App| {
                    let (x, y) = *pos;
                    let _ = this.update(cx, |this, cx| {
                        this.open_tap_tempo_menu(window, x, y, cx);
                    });
                },
            )
        };

        let on_ts_menu: components::BpmMenuCb = {
            let this = cx.entity().clone();
            Arc::new(
                move |pos: &(f32, f32), window: &mut Window, cx: &mut gpui::App| {
                    let (x, y) = *pos;
                    let _ = this.update(cx, |this, cx| {
                        this.open_time_signature_menu(window, x, y, cx);
                    });
                },
            )
        };

        let on_ts_edit_start: components::ChromeActionCb = {
            let this = cx.entity().clone();
            Arc::new(move |_: &(), _window: &mut Window, cx: &mut gpui::App| {
                let _ = this.update(cx, |this, cx| {
                    this.begin_ts_edit(None, cx);
                });
            })
        };

        let bpm_mouse_callbacks =
            bind_mouse_selection(cx.entity().clone(), |layout: &mut StudioLayout| {
                &mut layout.tempo_edit.bpm_input
            });
        let bpm_input_callbacks = TextInputCallbacks {
            on_context_command: None,
            on_context_menu: Some(transport_text_context(
                cx.entity().clone(),
                crate::layout::studio_state::TextMenuTarget::TransportBpm,
            )),
            on_mouse: bpm_mouse_callbacks.on_mouse,
        };
        // The master strip's gesture is rebuilt here rather than inside the
        // meter entity: this runs at the shell's render rate, while the meter
        // repaints on every poll tick and must not allocate a callback bundle
        // each time.
        let master_volume = self.build_master_volume_callbacks(cx.entity().clone());
        let _ = self
            .master_transport_meter
            .update(cx, |meter, _| meter.set_callbacks(master_volume));

        let ts_num_input_callbacks = bind_time_signature_mouse_selection(cx.entity().clone(), true);
        let ts_den_input_callbacks =
            bind_time_signature_mouse_selection(cx.entity().clone(), false);

        components::TransportChromeState {
            playing,
            recording,
            count_in_enabled,
            loop_enabled,
            metronome_enabled,
            follow_playhead,
            auto_scroll_continuous,
            position_label,
            bpm: bpm_value,
            bpm_label,
            bpm_has_automation,
            bpm_editing: self.tempo_edit.bpm_editing,
            bpm_input: self.tempo_edit.bpm_input.clone(),
            bpm_input_callbacks,
            // The layout's key handler routes keys while editing, so render the
            // caret whenever the editor is open.
            bpm_edit_focused: self.tempo_edit.bpm_editing,
            tap_tempo_session_taps: self.tap_tempo.tap_count().min(u8::MAX as usize) as u8,
            time_signature_label,
            ts_has_markers,
            ts_editing: self.tempo_edit.ts_editing,
            ts_num_input: self.tempo_edit.ts_num_input.clone(),
            ts_num_input_callbacks,
            ts_den_input: self.tempo_edit.ts_den_input.clone(),
            ts_den_input_callbacks,
            ts_edit_focus_num: self.tempo_edit.ts_edit_focus_num,
            on_ts_menu,
            on_ts_edit_start,
            on_return_to_start,
            on_play_toggle,
            on_stop,
            on_record,
            on_count_in_toggle,
            on_count_in_menu,
            on_loop_toggle,
            on_metronome_toggle,
            on_follow_toggle,
            on_follow_mode_toggle,
            on_set_bpm,
            on_bpm_drag,
            on_bpm_drag_end,
            on_bpm_menu,
            on_bpm_edit_start,
            on_tap_tempo,
            on_tap_tempo_menu,
            master_meter: Some(self.master_transport_meter.clone().into()),
            perf_meter: Some(self.transport_perf_meter.clone().into()),
        }
    }

    pub(super) fn tap_tempo_now(&mut self, cx: &mut Context<Self>) {
        let now = tap_tempo_now_secs();
        if let Some(bpm) = self.tap_tempo.tap(now) {
            self.apply_calculated_tap_bpm(bpm as f32, cx);
        }
        cx.notify();
    }

    pub(super) fn reset_tap_tempo(&mut self, cx: &mut Context<Self>) {
        self.tap_tempo.reset();
        cx.notify();
    }

    fn apply_calculated_tap_bpm(&mut self, bpm: f32, cx: &mut Context<Self>) {
        let target_point_id = {
            let state = &self.timeline.read(cx).state;
            if state.tempo_has_automation() {
                let beat = state.transport.playhead_beats as f64;
                state
                    .tempo_map
                    .point_id_at_or_before_beat(beat)
                    .map(|id| id.to_string())
            } else {
                None
            }
        };
        // One undo entry per tap session, not per tap. The first tap produces no
        // BPM, so the second is where a session first writes tempo: that tap
        // pushes the entry, and every later tap in the session extends it. A
        // timeout resets the tap count, so the next session starts a new entry.
        let first_commit_of_session = self.tap_tempo.tap_count() <= 2;
        let prev = first_commit_of_session.then(|| self.capture_tempo_state(cx));
        self.apply_bpm_value(bpm, target_point_id.as_deref(), true, cx);
        match prev {
            Some(prev) => {
                self.record_tempo_edit(TAP_TEMPO_EDIT_LABEL, prev, cx);
            }
            None => {
                if !self.amend_tempo_edit(TAP_TEMPO_EDIT_LABEL, cx) {
                    // The newest entry is no longer this session's (an edit
                    // landed in between): start a fresh one rather than
                    // rewriting somebody else's step.
                    let prev = self.capture_tempo_state(cx);
                    self.record_tempo_edit(TAP_TEMPO_EDIT_LABEL, prev, cx);
                }
            }
        }
    }

    pub(super) fn add_tempo_marker_from_current_tempo_at_playhead(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.add_tempo_marker_at_playhead(cx);
    }

    pub(super) fn open_tap_tempo_menu(
        &mut self,
        window: &Window,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.try_open_context_menu(
            ContextMenuRequest::from_window(
                window,
                x,
                y,
                ContextMenuTarget::Extended(ContextTarget::TapTempo),
            ),
            cx,
        );
    }

    pub(super) fn tap_tempo_shortcut_blocked(&self, window: &Window) -> bool {
        self.text_input_has_focus(window)
            || self.keyboard_text_capture_live(window)
            || self.tempo_edit.bpm_editing
            || self.tempo_edit.ts_editing
            || self.command_palette.is_open
            || self.project_switcher.is_open
            || self.plugin_picker.is_open
            || self.focused_text_input_is_composing(window)
    }

    fn focused_text_input_is_composing(&self, window: &Window) -> bool {
        [
            &self.command_palette_input,
            &self.project_switcher_search_input,
            &self.browser_search_input,
            &self.plugin_picker_search_input,
            &self.inspector_name_edit.name_input,
            &self.inspector_name_edit.clip_name_input,
            &self.tempo_edit.bpm_input,
            &self.tempo_edit.ts_num_input,
            &self.tempo_edit.ts_den_input,
        ]
        .into_iter()
        .any(|input| input.is_focused(window) && input.is_composing())
    }

    pub(super) fn status_text(&self) -> (String, String) {
        let content = self.status_bar_content(false);
        (content.left, content.audio)
    }

    pub(super) fn status_bar_content(&self, show_perf_metrics: bool) -> StatusBarContent {
        // Coalesced dropout notice — outranks the idle/playing labels but never a
        // recording status or a hard audio error.
        let dropout_active = self
            .audio_bridge
            .dropout_notice_until
            .is_some_and(|until| until > std::time::Instant::now());
        // Transient sample-rate notice (e.g. active != requested after a
        // re-open). Non-blocking — outranks idle labels, below recording/errors.
        let sample_rate_notice = self
            .audio_bridge
            .sample_rate_notice_until
            .is_some_and(|until| until > std::time::Instant::now())
            .then(|| self.audio_bridge.sample_rate_notice_text.clone())
            .filter(|text| !text.is_empty());
        // Aggregated Audio Connections / routing warnings — one line for the
        // whole project, never one dialog per affected track.
        let routing_notice = self
            .audio_bridge
            .routing_warnings
            .active_notice()
            .map(str::to_string);
        let left = match (
            self.recording.ui_state.status_text(),
            &self.audio_bridge.last_error,
            &self.audio_bridge.stats,
        ) {
            (Some(status), _, _) => status,
            (None, Some(error), _) => format!("Audio: {error}"),
            (None, None, _) if routing_notice.is_some() => {
                routing_notice.clone().unwrap_or_default()
            }
            (None, None, _) if sample_rate_notice.is_some() => {
                sample_rate_notice.clone().unwrap_or_default()
            }
            (None, None, _) if dropout_active => "Audio dropout detected".to_string(),
            (None, _, Some(stats)) if stats.transport_playing => "Playing".to_string(),
            (None, _, Some(stats)) if stats.running => "Audio ready".to_string(),
            (None, _, _) => "Ready".to_string(),
        };
        StatusBarContent {
            left,
            audio: self.status_audio_label(),
            perf: if show_perf_metrics {
                Some(self.status_perf_metrics())
            } else {
                None
            },
        }
    }

    fn status_audio_label(&self) -> String {
        let Some(stats) = self.audio_bridge.stats.as_ref() else {
            return "Audio offline".to_string();
        };
        let mut parts = vec![format!("{} Hz", stats.sample_rate.max(1))];
        if !stats.backend_name.is_empty() {
            parts.push(stats.backend_name.clone());
        }
        parts.push(format!(
            "Latency {:.1} ms",
            (stats.estimated_latency_ms * 10.0).round() / 10.0
        ));
        if stats.buffer_size > 0 {
            parts.push(format!("Buffer {}", stats.buffer_size));
        }
        parts.join("  ")
    }

    fn status_perf_metrics(&self) -> StatusBarPerfMetrics {
        StatusBarPerfMetrics {
            pill_label: self.frame_diag.compact_pill_label(),
            renderer:
                crate::components::timeline::timeline_surface::active_timeline_renderer_backend()
                    .to_string(),
            display_sync: self.frame_scheduler.describe(),
            fps: self.frame_diag.displayed_fps(),
            frame_ms: self.frame_diag.displayed_avg_ms(),
            peak_ms: self.frame_diag.displayed_peak_ms(),
            has_sample: self.frame_diag.has_sample(),
        }
    }

    pub(super) fn performance_overlay_snapshot(
        &self,
        repaint_reason: &str,
    ) -> PerformanceOverlaySnapshot {
        PerformanceOverlaySnapshot {
            renderer:
                crate::components::timeline::timeline_surface::active_timeline_renderer_backend()
                    .to_string(),
            display_sync: self.frame_scheduler.describe(),
            fps: self.frame_diag.displayed_fps(),
            frame_ms: self.frame_diag.displayed_avg_ms(),
            peak_ms: self.frame_diag.displayed_peak_ms(),
            has_sample: self.frame_diag.has_sample(),
            repaint_reason: repaint_reason.to_string(),
            audio: self.status_audio_label(),
            top_scopes: crate::perf::top_scopes(4),
            ui_cpu_ms: crate::perf::instrumented_cpu_ms_per_frame(),
            build_stamp: crate::perf::running_build_stamp().to_string(),
        }
    }

    pub(super) fn frame_reason(&self) -> &'static str {
        let playing = self
            .audio_bridge
            .stats
            .as_ref()
            .map(|s| s.transport_playing)
            .unwrap_or(false);
        if playing {
            return "transport";
        }
        if self.bottom_panel_state.is_resizing {
            return "panel-resize";
        }
        if self.overlay.open_popover.is_some() || self.menu_bar.open_menu_id.is_some() {
            return "menu";
        }
        "idle/interaction"
    }
}

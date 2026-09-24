//! Split out of `timeline.rs`: `impl Render for Timeline` + scrollbar/overlay helpers.

use super::*;

/// `FUTUREBOARD_PLAYHEAD_DEBUG=1` — trace the ruler playhead x-position each
/// frame. Cached so the per-frame render doesn't hit the OS env store while
/// the transport is running.
fn playhead_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLAYHEAD_DEBUG").is_some())
}

impl Render for Timeline {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _tl_scope = crate::perf::PerfScope::enter("Timeline");
        // A plug-in drag released anywhere but here leaves no event on this
        // element; drop its hint once no drag is in flight.
        if self.plugin_drop_hint.is_some() && !cx.has_active_drag() {
            self.plugin_drop_hint = None;
        }
        // `Timeline` is the arrangement's whole render and it dominates the
        // profiler's hot-scope list, which makes it the least useful entry on
        // it: knowing the arrangement is expensive says nothing about which
        // third of it to look at. These split it into the three phases it
        // actually has — resolving this frame's geometry, building the ~60
        // gesture callbacks the tree needs, and assembling the tree — so the
        // next reading names one of them instead.
        let mut phase = crate::perf::PerfScope::enter("Timeline:geometry");
        // One arrangement row layout per repaint. The build is O(track_count)
        // and clones every track id, so the scroll geometry, the GPUI track
        // list, and the arrangement snapshot all share this instance instead of
        // rebuilding it three times. Row heights cannot change during a repaint;
        // only `scroll_y` moves below, and it is refreshed after clamping.
        // Fold in the lane origin measured during the previous frame's prepaint
        // before anything reads the viewport. Every gesture context and lane
        // closure built below then resolves through the same x the ruler was
        // actually drawn at.
        if let Some(measured) = self.lane_origin_probe.get() {
            self.state.viewport.lane_origin_x_measured = Some(measured);
        }
        let mut row_layout = self.state.track_row_layout();
        let (viewport_w, viewport_h, (scroll_max_x, scroll_max_y)) =
            self.scroll_geometry_with_content_height(window, row_layout.total_height);
        self.state.update_viewport_size(viewport_w, viewport_h);
        self.state.clamp_scroll(scroll_max_x, scroll_max_y);
        let scrolling = self.state.smooth_scroll_towards_target();
        if scrolling {
            cx.notify();
        }
        row_layout.scroll_y = self.state.viewport.scroll_y;
        // One meter entity per track that has a header. Built here because this
        // is where the track set is known, and pruned in the same pass so a
        // deleted track does not leave its entity behind.
        {
            let ids: std::collections::HashSet<&str> =
                self.state.tracks.iter().map(|t| t.id.as_str()).collect();
            self.track_meters.retain(|id, _| ids.contains(id.as_str()));
            for index in 0..self.state.tracks.len() {
                let id = self.state.tracks[index].id.clone();
                if self.track_meters.contains_key(&id) {
                    continue;
                }
                let meter =
                    cx.new(|_| crate::components::timeline::vu_meter::TrackMeterView::new());
                self.track_meters.insert(id, meter);
            }
            // Same lifecycle for the cached lane views: one per track that has
            // a row, pruned in the same pass so a deleted track does not leave
            // its view behind.
            self.track_lanes.retain(|id, _| ids.contains(id.as_str()));
            let timeline = cx.entity();
            for index in 0..self.state.tracks.len() {
                let id = self.state.tracks[index].id.clone();
                if self.track_lanes.contains_key(&id) {
                    continue;
                }
                let lane_id = id.clone();
                let lane = cx.new(|cx| {
                    crate::components::timeline::track_lane_view::TrackLaneView::new(
                        &timeline, lane_id, cx,
                    )
                });
                self.track_lanes.insert(id, lane);
            }
        }
        if self.playhead_overlay.is_none() {
            // Built on first render for the same reason the focus subscription
            // below is: `Timeline::new` has no context to create an entity in.
            let frame = self.playhead_frame.clone();
            self.playhead_overlay = Some(cx.new(|_| {
                crate::components::timeline::playhead::PlayheadOverlay::new(
                    frame,
                    RULER_HEIGHT,
                    HEADER_WIDTH,
                )
            }));
        }
        if self.arrangement_surface.is_none() {
            let timeline = cx.entity();
            self.arrangement_surface = Some(cx.new(|cx| {
                crate::components::timeline::timeline_surface::TimelineSurfaceView::new(
                    &timeline, cx,
                )
            }));
        }
        if self.focus_lost_subscription.is_none() {
            self.focus_lost_subscription = Some(cx.on_focus_lost(window, |this, _window, cx| {
                if this.range_select_drag.is_some()
                    || this.pen_clip_draw.is_some()
                    || this.erase_clip_drag.is_some()
                    || this.automation_drag.is_some()
                    || this.automation_marquee.is_some()
                    || this.tempo_drag.is_some()
                    || this.song_text_drag_preview.is_some()
                    || this.pan_last_position.is_some()
                {
                    if Self::input_debug_enabled() {
                        eprintln!("[selection] focus_lost_cancel");
                    }
                    this.cancel_active_gesture(cx);
                }
            }));
        }

        // The arrangement has just resolved this frame's scroll and zoom, so it
        // is the only place that can put the playhead back in step with them.
        // Written straight to the cell: the overlay is about to be rendered
        // anyway, and notifying it here would queue a second pass.
        self.playhead_frame
            .set(crate::components::timeline::playhead::PlayheadFrame {
                x: self.state.beats_to_x(self.state.transport.playhead_beats),
            });
        if playhead_debug_enabled() {
            eprintln!(
                "[playhead x] beat={:.3} scroll_x={:.1} px_per_beat={:.3} x={:.1}",
                self.state.transport.playhead_beats,
                self.state.viewport.scroll_x,
                self.state.viewport.pixels_per_beat,
                self.playhead_frame.get().x
            );
        }
        let playhead_overlay = self.playhead_overlay.clone();

        // Diagnostic only, and it walks every track: skip the sum entirely when
        // the perf collector is off.
        if crate::perf::enabled() {
            crate::perf::count(
                "clips",
                self.state
                    .tracks
                    .iter()
                    .map(|t| t.clips.len() as u64)
                    .sum::<u64>(),
            );
        }

        phase = {
            drop(phase);
            crate::perf::PerfScope::enter("Timeline:callbacks")
        };

        let on_select_track = cx.listener(|this, track_id: &String, _window, cx| {
            this.state.select_track(track_id);
            cx.notify();
        });
        let on_select_track_header = cx.listener(
            |this, (track_id, additive, range): &(String, bool, bool), _window, cx| {
                this.state
                    .select_track_with_modifiers(track_id, *additive, *range);
                cx.notify();
            },
        );

        let on_select_clip = cx.listener(
            |this, (clip_id, additive, clone_drag): &(String, bool, bool), _window, cx| {
                if this.state.active_tool == TimelineTool::Pen {
                    let is_audio = this
                        .state
                        .find_clip(clip_id)
                        .map(|(_, clip)| matches!(clip.clip_type, ClipType::Audio { .. }))
                        .unwrap_or(false);
                    if is_audio {
                        if let Some((track_id, clip)) =
                            this.state.build_clip_duplicate_after(clip_id)
                        {
                            this.run_edit_command(EditCommand::CreateClip { track_id, clip }, cx);
                        }
                        return;
                    }
                }
                this.clip_clone_drag_id = clone_drag.then(|| clip_id.clone());
                this.clip_clone_hint = None;
                if *additive {
                    this.state.select_clip_additive(clip_id);
                } else {
                    // Plain click is an explicit single-clip selection. Multi-select is
                    // preserved only with Ctrl/Cmd additive clicks; this keeps imported
                    // multi-channel MIDI clips from behaving like an inseparable group.
                    this.state.select_clip(clip_id);
                }
                cx.notify();
            },
        );

        let on_toggle_mute = cx.listener(|this, track_id: &String, _window, cx| {
            this.state.toggle_track_mute(track_id);
            // Live control: reaches the engine via `on_track_param_change`
            // below; view-only dirty (no engine graph rebuild).
            this.mark_control_state_changed(cx);
            if let Some(track) = this.state.find_track(track_id) {
                if let Some(cb) = this.on_track_param_change.as_ref() {
                    // Engine param id is "muted" ("mute" hits the
                    // unknown-param branch and is dropped).
                    cb(
                        track_id.clone(),
                        "muted".to_string(),
                        if track.muted { 1.0 } else { 0.0 },
                    );
                }
            }
            cx.notify();
        });

        let on_toggle_solo = cx.listener(|this, track_id: &String, _window, cx| {
            this.state.toggle_track_solo(track_id);
            this.mark_control_state_changed(cx);
            if let Some(track) = this.state.find_track(track_id) {
                if let Some(cb) = this.on_track_param_change.as_ref() {
                    cb(
                        track_id.clone(),
                        "solo".to_string(),
                        if track.solo { 1.0 } else { 0.0 },
                    );
                }
            }
            cx.notify();
        });

        // Global latches in the Arrangement header. They clear, never set: the
        // per-track mutes they would otherwise overwrite are not recoverable
        // from anywhere. Each cleared track goes to the engine through the same
        // live param path a single M/S press uses, so the engine never has to
        // be told about a mute it already knows about.
        let on_clear_all_mutes = cx.listener(|this, _: &(), _window, cx| {
            let cleared = this.state.clear_all_track_mutes();
            if cleared.is_empty() {
                return;
            }
            if let Some(cb) = this.on_track_param_change.as_ref() {
                for track_id in &cleared {
                    cb(track_id.clone(), "muted".to_string(), 0.0);
                }
            }
            this.mark_control_state_changed(cx);
            cx.notify();
        });
        let on_clear_all_mutes: std::sync::Arc<dyn Fn(&(), &mut Window, &mut gpui::App) + 'static> =
            std::sync::Arc::new(on_clear_all_mutes);

        let on_clear_all_solos = cx.listener(|this, _: &(), _window, cx| {
            let cleared = this.state.clear_all_track_solos();
            if cleared.is_empty() {
                return;
            }
            if let Some(cb) = this.on_track_param_change.as_ref() {
                for track_id in &cleared {
                    cb(track_id.clone(), "solo".to_string(), 0.0);
                }
            }
            this.mark_control_state_changed(cx);
            cx.notify();
        });
        let on_clear_all_solos: std::sync::Arc<dyn Fn(&(), &mut Window, &mut gpui::App) + 'static> =
            std::sync::Arc::new(on_clear_all_solos);

        let on_toggle_arm = cx.listener(|this, track_id: &String, _window, cx| {
            let previous = this
                .state
                .find_track(track_id)
                .map(|track| (track.armed, track.input_monitor));
            if !this.state.toggle_track_arm(track_id) {
                return;
            }
            let next = this
                .state
                .find_track(track_id)
                .map(|track| (track.armed, track.input_monitor.is_active(track.armed)));
            let apply_error = next.and_then(|(armed, monitor)| {
                this.on_track_input_state_change
                    .as_ref()
                    .and_then(|cb| cb(track_id.clone(), armed, monitor).err())
            });
            if let Some(error) = apply_error {
                if let Some((armed, input_monitor)) = previous {
                    if let Some(track) = this
                        .state
                        .tracks
                        .iter_mut()
                        .find(|track| track.id == *track_id)
                    {
                        track.armed = armed;
                        track.input_monitor = input_monitor;
                    }
                }
                eprintln!("[audio] track input update rejected: {error}");
                cx.notify();
                return;
            }
            this.mark_control_state_changed(cx);
            cx.notify();
        });

        let on_toggle_input = cx.listener(|this, track_id: &String, _window, cx| {
            let previous = this
                .state
                .find_track(track_id)
                .map(|track| track.input_monitor);
            if !this.state.cycle_track_input_monitor(track_id) {
                return;
            }
            let next = this
                .state
                .find_track(track_id)
                .map(|track| (track.armed, track.input_monitor.is_active(track.armed)));
            let apply_error = next.and_then(|(armed, monitor)| {
                this.on_track_input_state_change
                    .as_ref()
                    .and_then(|cb| cb(track_id.clone(), armed, monitor).err())
            });
            if let Some(error) = apply_error {
                if let Some(input_monitor) = previous {
                    if let Some(track) = this
                        .state
                        .tracks
                        .iter_mut()
                        .find(|track| track.id == *track_id)
                    {
                        track.input_monitor = input_monitor;
                    }
                }
                eprintln!("[audio] track input update rejected: {error}");
                cx.notify();
                return;
            }
            this.mark_control_state_changed(cx);
            cx.notify();
        });

        // Automation mode toggle is UI-only: it selects the track and flips the
        // lane edit mode but never marks the project/engine dirty on its own.
        let on_toggle_automation = cx.listener(|this, track_id: &String, _window, cx| {
            this.state.select_track(track_id);
            this.state.toggle_track_lane_mode(track_id);
            cx.notify();
        });

        let on_volume_change =
            cx.listener(|this, (track_id, volume): &(String, f32), _window, cx| {
                this.state.set_track_volume(track_id, *volume);
                this.state.clear_track_volume_preview(track_id);
                // Double-click reset to 0 dB is the same live control edit a drag
                // is — the value goes down the realtime path on the next line, so
                // the engine graph must not be rebuilt for it. `mark_project_changed`
                // here made a single reset click cost a whole `load_project`.
                this.mark_control_state_changed(cx);
                if let Some(cb) = this.on_track_param_change.as_ref() {
                    cb(track_id.clone(), "volume".to_string(), *volume);
                }
                cx.notify();
            });
        let on_volume_drag_start =
            cx.listener(|this, (track_id, volume): &(String, f32), _window, cx| {
                this.state.begin_track_volume_preview(track_id, *volume);
                cx.notify();
            });
        let on_volume_drag_preview =
            cx.listener(|this, (track_id, volume): &(String, f32), _window, cx| {
                let changed = this.state.set_track_volume_preview(track_id, *volume);
                if changed {
                    crate::perf::count("fader_drag_preview_count", 1);
                    if let Some(cb) = this.on_track_param_change.as_ref() {
                        cb(track_id.clone(), "volume".to_string(), *volume);
                    }
                    cx.notify();
                }
            });
        let on_volume_drag_commit = cx.listener(|this, track_id: &String, _window, cx| {
            if let Some((prev, next)) = this.state.commit_track_volume_preview(track_id) {
                crate::perf::count("fader_drag_commit_count", 1);
                if (prev - next).abs() > 1.0e-5 {
                    this.record_executed_command(
                        EditCommand::SetTrackVolume {
                            track_id: track_id.clone(),
                            prev,
                            next,
                        },
                        cx,
                    );
                } else {
                    // Gesture that ended where it began: still a live control
                    // edit, never an engine-graph change. See `on_volume_change`.
                    this.mark_control_state_changed(cx);
                }
                if let Some(cb) = this.on_track_param_change.as_ref() {
                    cb(track_id.clone(), "volume".to_string(), next);
                }
                cx.notify();
            }
        });

        let on_pan_change = cx.listener(|this, (track_id, pan): &(String, f32), _window, cx| {
            let Some(prev) = this.state.find_track(track_id).map(|track| track.pan) else {
                return;
            };
            let next = pan.clamp(-1.0, 1.0);
            if (prev - next).abs() <= 1.0e-5 {
                return;
            }
            this.run_edit_command(
                EditCommand::SetTrackPan {
                    track_id: track_id.clone(),
                    prev,
                    next,
                },
                cx,
            );
            if let Some(cb) = this.on_track_param_change.as_ref() {
                cb(track_id.clone(), "pan".to_string(), next);
            }
        });
        let on_pan_drag_start = cx.listener(|this, track_id: &String, _window, cx| {
            this.state.begin_track_pan_preview(track_id);
            cx.notify();
        });
        let on_pan_drag_preview =
            cx.listener(|this, (track_id, pan): &(String, f32), _window, cx| {
                if this.state.set_track_pan_preview(track_id, *pan) {
                    if let Some(cb) = this.on_track_param_change.as_ref() {
                        cb(track_id.clone(), "pan".to_string(), *pan);
                    }
                    cx.notify();
                }
            });
        let on_pan_drag_commit = cx.listener(|this, track_id: &String, _window, cx| {
            if let Some((prev, next)) = this.state.commit_track_pan_preview(track_id) {
                if (prev - next).abs() > 1.0e-5 {
                    this.record_executed_command(
                        EditCommand::SetTrackPan {
                            track_id: track_id.clone(),
                            prev,
                            next,
                        },
                        cx,
                    );
                }
                if let Some(cb) = this.on_track_param_change.as_ref() {
                    cb(track_id.clone(), "pan".to_string(), next);
                }
                cx.notify();
            }
        });

        let on_add_clip = cx.listener(
            |this,
             (track_id, beat, click_count, bypass_snap): &(String, f32, u32, bool),
             _window,
             cx| {
                let track_type = this
                    .state
                    .tracks
                    .iter()
                    .find(|t| t.id == *track_id)
                    .map(|t| t.track_type);
                match track_type {
                    Some(TrackType::Audio) => {
                        if this.state.active_tool == TimelineTool::Pen {
                            let source_id = this
                                .state
                                .selection
                                .selected_clip_ids
                                .iter()
                                .find(|id| {
                                    this.state
                                        .find_clip(id)
                                        .map(|(_, clip)| {
                                            matches!(clip.clip_type, ClipType::Audio { .. })
                                        })
                                        .unwrap_or(false)
                                })
                                .cloned();
                            if let Some(source_id) = source_id {
                                let start = this.snap_beat_with_bypass(*beat, *bypass_snap);
                                this.create_clip_clone_at(&source_id, track_id, start, cx);
                            }
                        }
                    }
                    Some(TrackType::Midi | TrackType::Instrument) => {
                        if matches!(
                            this.state.active_tool,
                            TimelineTool::Pen | TimelineTool::Pointer
                        ) {
                            let start = this.snap_beat_with_bypass(*beat, *bypass_snap);
                            // Pen tool always commits a default-length clip on a
                            // plain click; the Pointer tool's empty-lane-creates
                            // gesture only commits on a drag or a double-click so
                            // marquee-style single clicks stay a no-op.
                            let commit_on_click =
                                this.state.active_tool == TimelineTool::Pen || *click_count >= 2;
                            this.pen_clip_draw = Some(ClipDrawPreview {
                                track_id: track_id.clone(),
                                start_beat: start,
                                current_beat: start,
                                dragging: false,
                                commit_on_click,
                            });
                        }
                    }
                    Some(
                        TrackType::Bus
                        | TrackType::Return
                        | TrackType::Group
                        | TrackType::Master
                        | TrackType::Video,
                    )
                    | None => {}
                }
                cx.notify();
            },
        );

        let on_audio_clip_process_preview = cx.listener(
            |this,
             (clip_id, update): &(
                String,
                crate::components::timeline::audio_clip::AudioClipProcessUpdate,
            ),
             _window,
             cx| {
                use crate::components::timeline::audio_clip::AudioClipProcessUpdate;
                let changed = match *update {
                    AudioClipProcessUpdate::Gain(gain) => this.state.set_clip_gain(clip_id, gain),
                    AudioClipProcessUpdate::FadeInMs(ms) => {
                        let Some(mut stretch) = this.state.clip_stretch(clip_id).cloned() else {
                            return;
                        };
                        stretch.fade_in_ms = ms.max(0.0);
                        stretch.dirty = true;
                        this.state.set_clip_stretch(clip_id, stretch)
                    }
                    AudioClipProcessUpdate::FadeOutMs(ms) => {
                        let Some(mut stretch) = this.state.clip_stretch(clip_id).cloned() else {
                            return;
                        };
                        stretch.fade_out_ms = ms.max(0.0);
                        stretch.dirty = true;
                        this.state.set_clip_stretch(clip_id, stretch)
                    }
                };
                if changed {
                    cx.notify();
                }
            },
        );
        let on_audio_clip_process_commit = cx.listener(
            |this, (clip_id, original): &(String, ClipState), _window, cx| {
                if let Some(next) = ClipSnapshot::capture(&this.state, clip_id) {
                    let previous = ClipSnapshot {
                        track_id: next.track_id.clone(),
                        clip: original.clone(),
                    };
                    if previous.clip != next.clip {
                        this.record_executed_command(
                            EditCommand::UpdateClip { previous, next },
                            cx,
                        );
                        this.mark_project_changed(cx);
                        this.mark_media_changed(cx);
                    }
                }
                cx.notify();
            },
        );

        let on_range_start = cx.listener(
            |this, (track_id, beat, additive): &(String, f32, bool), _window, cx| {
                if this.state.active_tool == TimelineTool::Pointer {
                    if Self::input_debug_enabled() {
                        eprintln!(
                            "[selection] marquee_start_pending track={} beat={:.3} additive={}",
                            track_id, beat, additive
                        );
                    }
                    this.range_select_drag = Some(RangeSelectDrag {
                        start_beat: *beat,
                        current_beat: *beat,
                        start_track_id: track_id.clone(),
                        additive: *additive,
                        dragging: false,
                    });
                    this.state.arrangement_range = None;
                    cx.notify();
                }
            },
        );

        let on_erase_start = cx.listener(|this, beat: &f32, _window, cx| {
            this.begin_erase_at(*beat, None, cx);
        });

        let on_erase_clip = cx.listener(|this, clip_id: &String, _window, cx| {
            let beat = this
                .state
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter())
                .find(|c| c.id == *clip_id)
                .map(|c| c.start_beat)
                .unwrap_or(0.0);
            this.begin_erase_at(beat, Some(clip_id.clone()), cx);
        });

        // Cut/razor tool: split the clicked audio clip at the cursor. The clip
        // element forwards the raw window x (it stops propagation, so the lane
        // never sees the click); resolve + snap it here where the timeline
        // geometry lives, then re-sync media so the split is audible.
        let on_cut_clip = cx.listener(
            |this, (clip_id, window_x, bypass): &(String, f32, bool), _window, cx| {
                let beat = this.beat_from_window_x(*window_x);
                let snapped = this.snap_beat_with_bypass(beat, *bypass);
                if this.split_audio_clip_at_beat(clip_id, snapped, cx) {
                    this.mark_media_changed(cx);
                }
            },
        );

        let on_edit_mouse_move = cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
            if event.pressed_button == Some(gpui::MouseButton::Left) {
                if let Some((anchor, start)) = this.floating_toolbar_drag_anchor {
                    let dx: f32 = (event.position.x - anchor.x).into();
                    let dy: f32 = (event.position.y - anchor.y).into();
                    this.floating_toolbar_position = Some(((start.0 + dx).max(0.0), (start.1 + dy).max(0.0)));
                    cx.notify();
                    return;
                }
            }
            if Self::input_debug_enabled() {
                eprintln!(
                    "[timeline-input] mouse-move pressed={:?} range_drag={} ctrl={} platform={} shift={}",
                    event.pressed_button,
                    this.range_select_drag.is_some(),
                    event.modifiers.control,
                    event.modifiers.platform,
                    event.modifiers.shift,
                );
            }
            if event.pressed_button.is_none()
                && (this.pen_clip_draw.is_some()
                    || this.range_select_drag.is_some()
                    || this.erase_clip_drag.is_some()
                    || this.automation_drag.is_some()
                    || this.automation_curve_drag.is_some()
                    || this.automation_marquee.is_some()
                    || this.tempo_drag.is_some()
                    || this.ts_drag.is_some()
                    || this.marker_drag.is_some()
                    || this.pan_last_position.is_some())
            {
                this.reset_input_state();
                cx.notify();
                return;
            }
            if event.pressed_button == Some(gpui::MouseButton::Left)
                && (this.automation_drag.is_some()
                    || this.automation_curve_drag.is_some()
                    || this.automation_marquee.is_some()
                    || this.tempo_drag.is_some()
                    || this.ts_drag.is_some()
                    || this.marker_drag.is_some())
            {
                if this.marker_drag.is_some() {
                    this.update_marker_track_interaction(event.position.x.into(), cx);
                } else if this.tempo_drag.is_some() {
                    this.update_tempo_track_interaction(
                        event.position.x.into(),
                        event.position.y.into(),
                        cx,
                    );
                } else if this.ts_drag.is_some() {
                    this.update_time_signature_track_interaction(
                        event.position.x.into(),
                        event.position.y.into(),
                        cx,
                    );
                } else {
                    this.update_automation_interaction(
                        event.position.x.into(),
                        event.position.y.into(),
                        event.modifiers.shift,
                        cx,
                    );
                }
                return;
            }
            if event.pressed_button == Some(gpui::MouseButton::Right)
                && this.erase_clip_drag.is_some()
            {
                let beat = this.snap_beat(this.beat_from_window_x(event.position.x.into()));
                this.update_erase_clip_drag(beat, cx);
            } else if event.pressed_button == Some(gpui::MouseButton::Left)
                && this.range_select_drag.is_some()
            {
                let beat = this.snap_beat(this.beat_from_window_x(event.position.x.into()));
                let lane_y = this.track_area_y_from_window(event.position);
                let current_track_id = this.state.lane_y_to_track_id(lane_y);
                let mut overlay: Option<TimelineRangeSelection> = None;
                if let Some(drag) = this.range_select_drag.as_mut() {
                    drag.current_beat = beat;
                    let dx = this.state.beats_to_x(beat) - this.state.beats_to_x(drag.start_beat);
                    let dy_tracks = current_track_id
                        .as_ref()
                        .map(|id| {
                            let row_layout = this.state.track_row_layout();
                            let start_row = row_layout.row_for_track(&drag.start_track_id);
                            let current_row = row_layout.row_for_track(id);
                            match (start_row, current_row) {
                                (Some(a), Some(b)) => (b.y - a.y).abs(),
                                _ => 0.0,
                            }
                        })
                        .unwrap_or(0.0);
                    if !drag.dragging && (dx * dx + dy_tracks * dy_tracks).sqrt() >= MARQUEE_DRAG_THRESHOLD {
                        drag.dragging = true;
                        if Self::input_debug_enabled() {
                            eprintln!("[selection] marquee_start additive={}", drag.additive);
                        }
                    }
                    if drag.dragging {
                        let (lo, hi) = normalize_range(drag.start_beat, beat);
                        let end_track_id = current_track_id.unwrap_or_else(|| drag.start_track_id.clone());
                        overlay = Some(TimelineRangeSelection::new(
                            lo as f64,
                            hi as f64,
                            this.state.track_ids_between(&drag.start_track_id, &end_track_id),
                        ));
                    }
                }
                this.state.arrangement_range = overlay;
                if Self::input_debug_enabled() {
                    if let Some(drag) = this.range_select_drag.as_ref() {
                        eprintln!(
                            "[selection] marquee_update dragging={} beat={:.3}",
                            drag.dragging, drag.current_beat
                        );
                    }
                }
                cx.notify();
            } else if event.pressed_button == Some(gpui::MouseButton::Left)
                && this.pen_clip_draw.is_some()
            {
                // Live MIDI clip draw: track the snapped cursor beat so the ghost
                // preview expands/shrinks in real time. No project mutation —
                // the real clip is created once on release. `pen_clip_draw` is
                // only ever populated for the tool/lane combos that should draw
                // (Pen on any MIDI/Instrument lane, or Pointer on an empty one),
                // so no extra tool check is needed here.
                let bypass_snap = event.modifiers.shift;
                let beat = this.snap_beat_with_bypass(
                    this.beat_from_window_x(event.position.x.into()),
                    bypass_snap,
                );
                if let Some(preview) = this.pen_clip_draw.as_mut() {
                    if (beat - preview.current_beat).abs() > f32::EPSILON {
                        preview.current_beat = beat;
                        if (beat - preview.start_beat).abs() > f32::EPSILON {
                            preview.dragging = true;
                        }
                        cx.notify();
                    }
                }
            }
        });

        let on_pen_mouse_up = cx.listener(|this, event: &gpui::MouseUpEvent, _window, cx| {
            if this.floating_toolbar_drag_anchor.take().is_some() {
                cx.notify();
                return;
            }
            this.log_input_state("mouse-up-left");
            let finished_marker = this.finish_marker_track_interaction(cx);
            let finished_tempo = this.finish_tempo_track_interaction(cx);
            let finished_ts = this.finish_time_signature_track_interaction(cx);
            let finished_automation = this.finish_automation_interaction(cx);
            if !finished_marker && !finished_tempo && !finished_ts && !finished_automation {
                let beat = this.snap_beat_with_bypass(
                    this.beat_from_window_x(event.position.x.into()),
                    event.modifiers.shift,
                );
                if this.pen_clip_draw.is_some() {
                    this.finish_pen_midi_clip(beat, cx);
                } else if this.range_select_drag.is_some() {
                    this.finish_range_select(beat, cx);
                } else if this.erase_clip_drag.is_some() {
                    this.finish_erase_clip_drag(cx);
                }
            }
            this.reset_input_state();
            debug_assert!(this.range_select_drag.is_none());
            cx.notify();
        });
        let on_pen_mouse_up_out = cx.listener(|this, event: &gpui::MouseUpEvent, _window, cx| {
            if this.floating_toolbar_drag_anchor.take().is_some() {
                cx.notify();
                return;
            }
            this.log_input_state("mouse-up-left-out");
            let finished_marker = this.finish_marker_track_interaction(cx);
            let finished_tempo = this.finish_tempo_track_interaction(cx);
            let finished_ts = this.finish_time_signature_track_interaction(cx);
            let finished_automation = this.finish_automation_interaction(cx);
            if !finished_marker && !finished_tempo && !finished_ts && !finished_automation {
                let beat = this.snap_beat_with_bypass(
                    this.beat_from_window_x(event.position.x.into()),
                    event.modifiers.shift,
                );
                if this.pen_clip_draw.is_some() {
                    this.finish_pen_midi_clip(beat, cx);
                } else if this.range_select_drag.is_some() {
                    this.finish_range_select(beat, cx);
                } else if this.erase_clip_drag.is_some() {
                    this.finish_erase_clip_drag(cx);
                }
            }
            this.reset_input_state();
            debug_assert!(this.range_select_drag.is_none());
            cx.notify();
        });

        let on_add_track = cx.listener(|this, _: &(), window, cx| {
            if let Some(callback) = this.on_add_track.as_ref() {
                callback(
                    &TimelineAddTrackRequest {
                        track_count: this.state.tracks.len(),
                        has_master_track: this
                            .state
                            .tracks
                            .iter()
                            .any(|track| track.track_type == TrackType::Master),
                    },
                    window,
                    cx,
                );
            } else {
                let id = this.state.create_audio_track();
                this.state.select_track(&id);
                cx.notify();
            }
        });

        let on_toggle_snap = cx.listener(|this, _: &(), _window, cx| {
            this.state.snap_to_grid = !this.state.snap_to_grid;
            cx.notify();
        });

        let on_cycle_grid = cx.listener(|this, _: &(), _window, cx| {
            // Cycle shape (Straight → Dotted → Triplet) before advancing the
            // base division so arrangement shares the same snap surface as the
            // piano roll without a separate control.
            use crate::components::timeline::timeline_state::SnapShape;
            match this.state.snap_shape {
                SnapShape::Straight => {
                    this.state.snap_shape = SnapShape::Dotted;
                }
                SnapShape::Dotted => {
                    this.state.snap_shape = SnapShape::Triplet;
                }
                SnapShape::Triplet => {
                    this.state.snap_shape = SnapShape::Straight;
                    this.state.grid_division = match this.state.grid_division {
                        SnapDivision::Auto => SnapDivision::Off,
                        SnapDivision::Off => SnapDivision::Bar1,
                        SnapDivision::Bar1 => SnapDivision::Div1_1,
                        SnapDivision::Div1_1 => SnapDivision::Div1_2,
                        SnapDivision::Div1_2 => SnapDivision::Div1_4,
                        SnapDivision::Div1_4 => SnapDivision::Div1_8,
                        SnapDivision::Div1_8 => SnapDivision::Div1_16,
                        SnapDivision::Div1_16 => SnapDivision::Div1_32,
                        SnapDivision::Div1_32 => SnapDivision::Div1_64,
                        SnapDivision::Div1_64 => SnapDivision::Auto,
                    };
                }
            }
            cx.notify();
        });

        let timeline_seek = cx.entity().clone();
        let on_seek: std::sync::Arc<
            dyn Fn(&f32, crate::layout::SeekReason, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(move |click_x, reason, _window, cx| {
            let _ = timeline_seek.update(cx, |timeline, cx| {
                let beats = timeline.state.x_to_beats(*click_x);
                timeline.seek_to_beat_with_reason(beats, reason, cx);
            });
        });

        let on_select_tool = cx.listener(|this, tool: &TimelineTool, _window, cx| {
            this.log_input_state("tool-change");
            this.reset_input_state();
            this.state.active_tool = *tool;
            cx.notify();
        });

        // Smooth, continuous zoom factor — small per-click multiplier so the
        // px/bt label changes feel like a real ramp rather than a jump.
        // Anchor at the viewport center (no cursor info here) so zoom stays
        // visually stable when driven from the buttons.
        let on_zoom_in = cx.listener(|this, _: &(), window, cx| {
            let viewport_w: f32 = window.bounds().size.width.into();
            let anchor = ((viewport_w - this.state.lane_origin_x()) * 0.5).max(0.0);
            this.state.zoom_by(1.35, anchor);
            cx.notify();
        });

        let on_zoom_out = cx.listener(|this, _: &(), window, cx| {
            let viewport_w: f32 = window.bounds().size.width.into();
            let anchor = ((viewport_w - this.state.lane_origin_x()) * 0.5).max(0.0);
            this.state.zoom_by(1.0 / 1.35, anchor);
            cx.notify();
        });

        // Wrap callbacks in std::sync::Arc to allow easy cloning when passing down to sub-elements
        let on_select_track: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_select_track);
        let on_select_clip: std::sync::Arc<
            dyn Fn(&(String, bool, bool), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_select_clip);
        let on_toggle_mute: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_toggle_mute);
        let on_toggle_solo: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_toggle_solo);
        let on_toggle_arm: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_toggle_arm);
        let on_toggle_input: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_toggle_input);
        let on_toggle_automation: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_toggle_automation);
        let on_volume_change: std::sync::Arc<
            dyn Fn(&(String, f32), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_volume_change);
        let on_volume_drag_start: std::sync::Arc<
            dyn Fn(&(String, f32), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_volume_drag_start);
        let on_volume_drag_preview: std::sync::Arc<
            dyn Fn(&(String, f32), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_volume_drag_preview);
        let on_volume_drag_commit: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_volume_drag_commit);
        let on_pan_change: std::sync::Arc<
            dyn Fn(&(String, f32), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_pan_change);
        let on_pan_drag_start: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_pan_drag_start);
        let on_pan_drag_preview: std::sync::Arc<
            dyn Fn(&(String, f32), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_pan_drag_preview);
        let on_pan_drag_commit: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_pan_drag_commit);
        let on_add_clip: std::sync::Arc<
            dyn Fn(&(String, f32, u32, bool), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_add_clip);
        let on_audio_clip_process_preview:
            crate::components::timeline::audio_clip::AudioClipProcessPreviewCb =
            std::sync::Arc::new(on_audio_clip_process_preview);
        let on_audio_clip_process_commit:
            crate::components::timeline::audio_clip::AudioClipProcessCommitCb =
            std::sync::Arc::new(on_audio_clip_process_commit);
        let on_add_track: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static> =
            std::sync::Arc::new(on_add_track);
        let on_toggle_snap: std::sync::Arc<
            dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_toggle_snap);
        let on_cycle_grid: std::sync::Arc<
            dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_cycle_grid);
        let on_seek = on_seek.clone();
        let on_playhead_scrub_begin = self.on_playhead_scrub_begin.clone();
        let on_playhead_scrub_end = self.on_playhead_scrub_end.clone();

        // Right-click on the ruler → position-aware tempo menu. Converts the
        // markings-area-local x to a beat and forwards the screen position so
        // the overlay anchors under the cursor.
        let on_ruler_context = cx.listener(
            |this, payload: &(f32, f32, f32), window: &mut gpui::Window, cx| {
                let (click_x, sx, sy) = *payload;
                let beat = this.state.x_to_beats(click_x).max(0.0) as f64;
                if let Some(cb) = this.on_context_menu.clone() {
                    cb(&(TimelineContextTarget::Ruler(beat), sx, sy), window, cx);
                }
            },
        );
        let on_song_text_marker_down =
            cx.listener(|this, marker: &SongTextMarkerDown, window, cx| {
                this.song_text_drag_cancelled = false;
                this.state
                    .select_song_text_event(&marker.event_id, marker.additive);
                this.seek_to_exact_beat(
                    marker.beat as f32,
                    crate::layout::SeekReason::TimelineClick,
                    cx,
                );
                if marker.click_count >= 2 {
                    if let Some(open) = this.on_open_song_text_editor.as_ref() {
                        open(window, cx);
                    }
                }
            });
        let on_song_text_marker_down: crate::components::timeline::song_text_track::SongTextMarkerDownCallback =
            std::sync::Arc::new(on_song_text_marker_down);
        let on_song_text_marker_context = self.on_context_menu.clone().map(|callback| {
            let timeline = cx.entity().clone();
            std::sync::Arc::new(
                move |(event_id, beat, x, y): &(String, f64, f32, f32),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    let _ = timeline.update(cx, |timeline, cx| {
                        timeline.state.select_song_text_event(event_id, false);
                        timeline.seek_to_exact_beat(
                            *beat as f32,
                            crate::layout::SeekReason::TimelineClick,
                            cx,
                        );
                    });
                    callback(
                        &(
                            TimelineContextTarget::SongTextMarker {
                                event_id: event_id.clone(),
                                beat: *beat,
                            },
                            *x,
                            *y,
                        ),
                        window,
                        cx,
                    );
                },
            )
                as crate::components::timeline::song_text_track::SongTextMarkerContextCallback
        });
        let on_song_text_empty_seek = cx.listener(|this, lane_x: &f32, _window, cx| {
            this.state.clear_song_text_selection();
            let beat = this.state.x_to_beat(*lane_x).max(0.0);
            this.seek_to_exact_beat(beat as f32, crate::layout::SeekReason::TimelineClick, cx);
        });
        let on_song_text_empty_seek: crate::components::timeline::song_text_track::SongTextLaneSeekCallback =
            std::sync::Arc::new(on_song_text_empty_seek);

        let on_region_drag = cx.listener(|this, update: &TimelineRegionDragUpdate, _window, cx| {
            // Snapshot before the first mutation of the gesture; the drop turns
            // it into one history entry for the whole drag rather than one per
            // mouse-move.
            if this.region_gesture_origin.is_none() {
                this.region_gesture_origin = Some(this.state.regions.clone());
            }
            if this
                .state
                .update_region_range(&update.region_id, update.start_beat, update.end_beat)
            {
                this.mark_project_changed(cx);
                cx.notify();
            }
        });
        let on_region_drag_drop = cx.listener(|this, _drag: &TimelineRegionDrag, _window, cx| {
            if let Some(prev) = this.region_gesture_origin.take() {
                this.record_region_edit("Move Region", prev, cx);
            }
            cx.notify();
        });
        let on_region_drag: std::sync::Arc<
            dyn Fn(&TimelineRegionDragUpdate, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_region_drag);

        // ── Chord Track ─────────────────────────────────────────────────
        let on_chord_drag = cx.listener(|this, update: &ChordEventDragUpdate, _window, cx| {
            if this.chord_gesture_origin.is_none() {
                this.chord_gesture_origin = Some(this.state.chord_events.clone());
            }
            // Every move restarts from the gesture's origin, so dragging a
            // chord across its neighbours trims them only where it finally
            // lands, not everywhere it passed.
            if let Some(origin) = this.chord_gesture_origin.clone() {
                this.state.chord_events = origin;
            }
            this.state
                .set_chord_event_range(update.event_id, update.start_beat, update.end_beat);
            this.state.select_chord_event(update.event_id);
            cx.notify();
        });
        let on_chord_drag: crate::components::timeline::chord_track::ChordTrackDragCallback =
            std::sync::Arc::new(on_chord_drag);
        let on_chord_drag_drop = cx.listener(|this, _drag: &ChordEventDrag, _window, cx| {
            if let Some(prev) = this.chord_gesture_origin.take() {
                this.record_chord_edit("Move Chord", prev, cx);
            }
            cx.notify();
        });
        let on_chord_down = cx.listener(|this, payload: &(f64, Option<u64>, u32), _window, cx| {
            this.begin_chord_track_interaction(payload.0, payload.1, payload.2, cx);
        });
        let on_chord_down: crate::components::timeline::chord_track::ChordTrackDownCallback =
            std::sync::Arc::new(on_chord_down);
        let on_chord_context = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |(beat, event_id, x, y): &(f64, Option<u64>, f32, f32),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    cb(
                        &(
                            TimelineContextTarget::ChordTrack {
                                beat: *beat,
                                event_id: *event_id,
                            },
                            *x,
                            *y,
                        ),
                        window,
                        cx,
                    );
                },
            ) as crate::components::timeline::chord_track::ChordTrackContextCallback
        });
        let on_chord_open_generator = cx.listener(|this, _: &(), _window, cx| {
            this.request_command("chords:open-generator", cx);
        });
        let on_chord_open_generator: crate::components::timeline::region_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_chord_open_generator);
        let on_chord_header_menu = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |pos: &(f32, f32), window: &mut gpui::Window, cx: &mut gpui::App| {
                    cb(&(TimelineContextTarget::ChordLaneHeader, pos.0, pos.1), window, cx);
                },
            ) as crate::components::timeline::region_track::GlobalLaneMenuCallback
        });
        let on_chord_hide = cx.listener(|this, _: &(), _window, cx| {
            this.state.hide_chord_track_lane();
            cx.notify();
        });
        let on_chord_hide: crate::components::timeline::region_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_chord_hide);
        let on_chord_toggle_collapsed = cx.listener(|this, _: &(), _window, cx| {
            this.state.chord_track_collapsed = !this.state.chord_track_collapsed;
            this.mark_control_state_changed(cx);
            cx.notify();
        });
        let on_chord_toggle_collapsed: crate::components::timeline::region_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_chord_toggle_collapsed);
        let on_loop_drag = cx.listener(|this, update: &TimelineLoopDragUpdate, _window, cx| {
            let start = update.start_beat.min(update.end_beat).max(0.0);
            let end = update.start_beat.max(update.end_beat).max(start + 1.0e-3);
            let transport = &mut this.state.transport;
            if (transport.loop_start_beats - start).abs() > f32::EPSILON
                || (transport.loop_end_beats - end).abs() > f32::EPSILON
            {
                transport.loop_start_beats = start;
                transport.loop_end_beats = end;
                transport.loop_enabled = true;
                this.mark_loop_changed(cx);
                cx.notify();
            }
        });
        let on_loop_drag: std::sync::Arc<
            dyn Fn(&TimelineLoopDragUpdate, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_loop_drag);
        let on_ruler_context: std::sync::Arc<
            dyn Fn(&(f32, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_ruler_context);
        let on_select_tool: std::sync::Arc<
            dyn Fn(&TimelineTool, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_select_tool);
        let on_range_start: std::sync::Arc<
            dyn Fn(&(String, f32, bool), &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_range_start);
        let on_erase_start: std::sync::Arc<
            dyn Fn(&f32, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_erase_start);
        let on_erase_clip: std::sync::Arc<
            dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static,
        > = std::sync::Arc::new(on_erase_clip);
        let on_cut_clip: crate::components::timeline::audio_clip::AudioClipCutCb =
            std::sync::Arc::new(on_cut_clip);
        let on_zoom_in: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static> =
            std::sync::Arc::new(on_zoom_in);
        let on_zoom_out: std::sync::Arc<dyn Fn(&(), &mut gpui::Window, &mut gpui::App) + 'static> =
            std::sync::Arc::new(on_zoom_out);
        let on_timeline_context = self.on_context_menu.clone();
        let on_track_context_menu = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |(track_id, x, y): &(String, f32, f32),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    cb(
                        &(TimelineContextTarget::TrackHeader(track_id.clone()), *x, *y),
                        window,
                        cx,
                    );
                },
            )
                as std::sync::Arc<
                    dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
                >
        });
        let on_clip_context_menu = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |(clip_id, x, y): &(String, f32, f32),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    cb(
                        &(TimelineContextTarget::Clip(clip_id.clone()), *x, *y),
                        window,
                        cx,
                    );
                },
            )
                as std::sync::Arc<
                    dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static,
                >
        });

        let on_automation_down = cx.listener(
            |this, payload: &(String, String, f32, f32, bool, bool, u32), _window, cx| {
                let (track_id, lane_id, beat, value, additive, alt, click_count) = (
                    payload.0.clone(),
                    payload.1.clone(),
                    payload.2,
                    payload.3,
                    payload.4,
                    payload.5,
                    payload.6,
                );
                this.begin_automation_interaction(
                    &track_id,
                    &lane_id,
                    beat,
                    value,
                    additive,
                    alt,
                    click_count >= 2,
                    cx,
                );
            },
        );
        let on_automation_down: crate::components::timeline::automation_lane::AutomationDownCallback =
            std::sync::Arc::new(on_automation_down);

        // Not a `cx.listener`: the lane needs the answer back to decide whether
        // to swallow the press, and a listener returns nothing.
        let on_automation_delete: crate::components::timeline::automation_lane::AutomationDeleteCallback = {
            let this = cx.entity().clone();
            std::sync::Arc::new(
                move |payload: &(String, String, f32, f32), _window, cx: &mut gpui::App| {
                    let (track_id, lane_id, beat, value) =
                        (payload.0.clone(), payload.1.clone(), payload.2, payload.3);
                    this.update(cx, |this, cx| {
                        this.delete_automation_point_at(&track_id, &lane_id, beat, value, cx)
                    })
                },
            )
        };

        // Hover resolver: a normal move resolves the point/segment under the
        // cursor; a negative-sentinel payload (hover-out) clears this lane's hover.
        let on_automation_hover =
            cx.listener(|this, payload: &(String, String, f32, f32), _window, cx| {
                let (track_id, lane_id, beat, value) =
                    (payload.0.clone(), payload.1.clone(), payload.2, payload.3);
                if beat < 0.0 {
                    this.clear_automation_hover_for_lane(&track_id, &lane_id, cx);
                } else {
                    this.update_automation_hover(&track_id, &lane_id, beat, value, cx);
                }
            });
        let on_automation_hover: crate::components::timeline::automation_lane::AutomationHoverCallback =
            std::sync::Arc::new(on_automation_hover);

        // Sub-lane header controls: activate / mode / clear / remove / pick.
        // Activation and the picker are UI-only; mode/clear/remove are
        // committed edits.
        let on_automation_lane_action = cx.listener(
            |this,
             payload: &(
                String,
                String,
                crate::components::timeline::automation_lane::AutomationLaneAction,
                f32,
                f32,
            ),
             window,
             cx| {
                use crate::components::timeline::automation_control_lane::AutomationControlAction;
                use crate::components::timeline::automation_lane::AutomationLaneAction;
                let (track_id, lane_id, action, x, y) = (
                    payload.0.clone(),
                    payload.1.clone(),
                    payload.2,
                    payload.3,
                    payload.4,
                );
                match action {
                    AutomationLaneAction::PickTarget => {
                        // The lane name is a parameter selector: the same picker
                        // the control row opens, anchored at the click.
                        this.state.activate_automation_lane(&track_id, &lane_id);
                        if let Some(cb) = this.on_automation_control.clone() {
                            cb(
                                &(
                                    track_id.clone(),
                                    AutomationControlAction::OpenTargetPicker,
                                    x,
                                    y,
                                ),
                                window,
                                cx,
                            );
                        }
                        cx.notify();
                    }
                    AutomationLaneAction::Activate => {
                        this.state.activate_automation_lane(&track_id, &lane_id);
                        this.state.select_track(&track_id);
                        cx.notify();
                    }
                    AutomationLaneAction::ToggleEnable => {
                        if this
                            .state
                            .toggle_automation_lane_enabled(&track_id, &lane_id)
                            .is_some()
                        {
                            this.mark_project_changed(cx);
                            cx.notify();
                        }
                    }
                    AutomationLaneAction::Clear => {
                        if this.state.clear_automation_lane(&track_id, &lane_id) > 0 {
                            this.mark_project_changed(cx);
                            cx.notify();
                        }
                    }
                    AutomationLaneAction::Hide => {
                        if this.state.remove_automation_lane(&track_id, &lane_id) {
                            this.mark_project_changed(cx);
                            cx.notify();
                        }
                    }
                }
            },
        );
        let on_automation_lane_action: crate::components::timeline::automation_lane::AutomationLaneActionCallback =
            std::sync::Arc::new(on_automation_lane_action);

        let on_automation_control = self.on_automation_control.clone();

        let on_tempo_down = cx.listener(
            |this, payload: &(f64, f64, Option<String>, bool, u32), window, cx| {
                let (beat, bpm, point_id, _additive, click_count) = (
                    payload.0,
                    payload.1,
                    payload.2.clone(),
                    payload.3,
                    payload.4,
                );
                // Double-click directly on an existing marker opens the
                // inline BPM editor for that marker instead of the
                // (previously dead) fallthrough that just re-selected it.
                if click_count >= 2 {
                    if let Some(id) = point_id.as_deref() {
                        if let Some(cb) = this.on_tempo_point_edit.clone() {
                            cb(id, window, cx);
                            return;
                        }
                    }
                }
                this.begin_tempo_track_interaction(beat, bpm, point_id, click_count, cx);
            },
        );
        let on_tempo_down: crate::components::timeline::tempo_track::TempoTrackDownCallback =
            std::sync::Arc::new(on_tempo_down);

        let on_tempo_context = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |(beat, bpm, point_id, x, y): &(f64, f64, Option<String>, f32, f32),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    cb(
                        &(
                            TimelineContextTarget::TempoTrack {
                                beat: *beat,
                                bpm: *bpm,
                                point_id: point_id.clone(),
                            },
                            *x,
                            *y,
                        ),
                        window,
                        cx,
                    );
                },
            ) as crate::components::timeline::tempo_track::TempoTrackContextCallback
        });

        let on_tempo_add = cx.listener(|this, _: &(), _window, cx| {
            this.add_tempo_point_at_playhead_from_header(cx);
        });
        let on_tempo_add: crate::components::timeline::tempo_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_tempo_add);

        let on_tempo_header_menu = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |pos: &(f32, f32), window: &mut gpui::Window, cx: &mut gpui::App| {
                    cb(
                        &(TimelineContextTarget::TempoLaneHeader, pos.0, pos.1),
                        window,
                        cx,
                    );
                },
            ) as crate::components::timeline::tempo_track::GlobalLaneMenuCallback
        });

        let on_tempo_hide = cx.listener(|this, _: &(), _window, cx| {
            this.state.hide_tempo_track_lane();
            cx.notify();
        });
        let on_tempo_hide: crate::components::timeline::tempo_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_tempo_hide);

        let on_song_text_hide = cx.listener(|this, _: &(), _window, cx| {
            this.state.hide_song_text_track_lane();
            cx.notify();
        });
        let on_song_text_hide: crate::components::timeline::song_text_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_song_text_hide);

        let on_tempo_toggle_collapsed = cx.listener(|this, _: &(), _window, cx| {
            this.state.tempo_track_collapsed = !this.state.tempo_track_collapsed;
            // Persisted with the project since v40, so folding a lane away is an
            // unsaved change — view-only, though: the audio graph is untouched.
            this.mark_control_state_changed(cx);
            cx.notify();
        });
        let on_tempo_toggle_collapsed: crate::components::timeline::tempo_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_tempo_toggle_collapsed);

        // ── Marker lane ─────────────────────────────────────────────────
        let on_marker_down = cx.listener(
            |this,
             down: &crate::components::timeline::marker_track::MarkerLaneDown,
             _window,
             cx| {
                this.begin_marker_track_interaction(down, cx);
            },
        );
        let on_marker_down: crate::components::timeline::marker_track::MarkerTrackDownCallback =
            std::sync::Arc::new(on_marker_down);

        let on_marker_context = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |(beat, marker_id, x, y): &(f64, Option<String>, f32, f32),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    cb(
                        &(
                            TimelineContextTarget::MarkerTrack {
                                beat: *beat,
                                marker_id: marker_id.clone(),
                            },
                            *x,
                            *y,
                        ),
                        window,
                        cx,
                    );
                },
            ) as crate::components::timeline::marker_track::MarkerTrackContextCallback
        });

        let on_marker_add = cx.listener(|this, _: &(), _window, cx| {
            this.add_marker_at_playhead_from_header(cx);
        });
        let on_marker_add: crate::components::timeline::marker_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_marker_add);

        let on_marker_header_menu = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |pos: &(f32, f32), window: &mut gpui::Window, cx: &mut gpui::App| {
                    cb(
                        &(TimelineContextTarget::MarkerLaneHeader, pos.0, pos.1),
                        window,
                        cx,
                    );
                },
            ) as crate::components::timeline::marker_track::GlobalLaneMenuCallback
        });

        let on_marker_hide = cx.listener(|this, _: &(), _window, cx| {
            this.state.hide_marker_track_lane();
            cx.notify();
        });
        let on_marker_hide: crate::components::timeline::marker_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_marker_hide);

        let on_marker_toggle_collapsed = cx.listener(|this, _: &(), _window, cx| {
            this.state.marker_track_collapsed = !this.state.marker_track_collapsed;
            // Persisted with the project since v40, so folding a lane away is an
            // unsaved change — view-only, though: the audio graph is untouched.
            this.mark_control_state_changed(cx);
            cx.notify();
        });
        let on_marker_toggle_collapsed: crate::components::timeline::marker_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_marker_toggle_collapsed);

        // ── Region lane ─────────────────────────────────────────────────
        let on_region_down =
            cx.listener(|this, payload: &(f64, Option<String>, u32), _window, cx| {
                let (beat, region_id, click_count) = (payload.0, payload.1.clone(), payload.2);
                this.begin_region_track_interaction(beat, region_id, click_count, cx);
            });
        let on_region_down: crate::components::timeline::region_track::RegionTrackDownCallback =
            std::sync::Arc::new(on_region_down);

        let on_region_context = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |(beat, region_id, x, y): &(f64, Option<String>, f32, f32),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    cb(
                        &(
                            TimelineContextTarget::RegionTrack {
                                beat: *beat,
                                region_id: region_id.clone(),
                            },
                            *x,
                            *y,
                        ),
                        window,
                        cx,
                    );
                },
            ) as crate::components::timeline::region_track::RegionTrackContextCallback
        });

        let on_region_add = cx.listener(|this, _: &(), _window, cx| {
            this.add_region_at_playhead_from_header(cx);
        });
        let on_region_add: crate::components::timeline::region_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_region_add);

        let on_region_header_menu = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |pos: &(f32, f32), window: &mut gpui::Window, cx: &mut gpui::App| {
                    cb(
                        &(TimelineContextTarget::RegionLaneHeader, pos.0, pos.1),
                        window,
                        cx,
                    );
                },
            ) as crate::components::timeline::region_track::GlobalLaneMenuCallback
        });

        let on_region_hide = cx.listener(|this, _: &(), _window, cx| {
            this.state.hide_region_track_lane();
            cx.notify();
        });
        let on_region_hide: crate::components::timeline::region_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_region_hide);

        let on_region_toggle_collapsed = cx.listener(|this, _: &(), _window, cx| {
            this.state.region_track_collapsed = !this.state.region_track_collapsed;
            // Persisted with the project since v40, so folding a lane away is an
            // unsaved change — view-only, though: the audio graph is untouched.
            this.mark_control_state_changed(cx);
            cx.notify();
        });
        let on_region_toggle_collapsed: crate::components::timeline::region_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_region_toggle_collapsed);

        let on_ts_down = cx.listener(
            |this, payload: &(f64, Option<String>, bool, u32), _window, cx| {
                let (beat, point_id, _additive, click_count) =
                    (payload.0, payload.1.clone(), payload.2, payload.3);
                this.begin_time_signature_track_interaction(beat, point_id, click_count, cx);
            },
        );
        let on_ts_down: crate::components::timeline::time_signature_track::TimeSignatureTrackDownCallback =
            std::sync::Arc::new(on_ts_down);

        let on_ts_context = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |(beat, point_id, x, y): &(f64, Option<String>, f32, f32),
                      window: &mut gpui::Window,
                      cx: &mut gpui::App| {
                    cb(
                        &(
                            TimelineContextTarget::TimeSignatureTrack {
                                beat: *beat,
                                point_id: point_id.clone(),
                            },
                            *x,
                            *y,
                        ),
                        window,
                        cx,
                    );
                },
            ) as crate::components::timeline::time_signature_track::TimeSignatureTrackContextCallback
        });

        let on_ts_add = cx.listener(|this, _: &(), _window, cx| {
            this.add_time_signature_marker_at_playhead_from_header(cx);
        });
        let on_ts_add: crate::components::timeline::time_signature_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_ts_add);

        let on_ts_header_menu = self.on_context_menu.clone().map(|cb| {
            std::sync::Arc::new(
                move |pos: &(f32, f32), window: &mut gpui::Window, cx: &mut gpui::App| {
                    cb(
                        &(TimelineContextTarget::TimeSignatureLaneHeader, pos.0, pos.1),
                        window,
                        cx,
                    );
                },
            )
                as crate::components::timeline::time_signature_track::GlobalLaneMenuCallback
        });

        let on_ts_hide = cx.listener(|this, _: &(), _window, cx| {
            this.state.hide_time_signature_track_lane();
            cx.notify();
        });
        let on_ts_hide: crate::components::timeline::time_signature_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_ts_hide);

        let on_ts_toggle_collapsed = cx.listener(|this, _: &(), _window, cx| {
            this.state.time_signature_track_collapsed = !this.state.time_signature_track_collapsed;
            // Persisted with the project since v40, so folding a lane away is an
            // unsaved change — view-only, though: the audio graph is untouched.
            this.mark_control_state_changed(cx);
            cx.notify();
        });
        let on_ts_toggle_collapsed: crate::components::timeline::time_signature_track::GlobalLaneVoidCallback =
            std::sync::Arc::new(on_ts_toggle_collapsed);

        let on_assign_to_group: std::sync::Arc<
            dyn Fn(&(String, String), &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |(track_id, group_id), _window, cx| {
                let _ = this.update(cx, |this, cx| {
                    if this.state.assign_track_to_group(track_id, group_id) {
                        this.mark_project_changed(cx);
                        cx.notify();
                    }
                });
            })
        };
        let on_toggle_group_collapsed: std::sync::Arc<
            dyn Fn(&String, &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |group_id, _window, cx| {
                let _ = this.update(cx, |this, cx| {
                    if this.state.toggle_group_collapsed(group_id).is_some() {
                        this.mark_project_changed(cx);
                        cx.notify();
                    }
                });
            })
        };

        let header_callbacks = crate::components::timeline::track_header::TrackHeaderCallbacks {
            on_select_track: std::sync::Arc::new(on_select_track_header),
            on_toggle_mute: on_toggle_mute.clone(),
            on_toggle_solo: on_toggle_solo.clone(),
            on_toggle_arm: on_toggle_arm.clone(),
            on_toggle_input: on_toggle_input.clone(),
            on_toggle_automation: on_toggle_automation.clone(),
            on_volume_change: on_volume_change.clone(),
            on_volume_drag_start: on_volume_drag_start.clone(),
            on_volume_drag_preview: on_volume_drag_preview.clone(),
            on_volume_drag_commit: on_volume_drag_commit.clone(),
            on_pan_change: on_pan_change.clone(),
            on_pan_drag_start: on_pan_drag_start.clone(),
            on_pan_drag_preview: on_pan_drag_preview.clone(),
            on_pan_drag_commit: on_pan_drag_commit.clone(),
            on_assign_to_group,
            on_toggle_group_collapsed,
            on_context_menu: on_track_context_menu.clone(),
            on_toggle_takes: {
                let this = cx.entity().clone();
                std::sync::Arc::new(move |track_id, _window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        // View state, not project content: opening the take
                        // list is not an edit and must not mark the project
                        // dirty. It is persisted with the track anyway, the way
                        // a folder's collapse state is.
                        if this.state.toggle_takes_expanded(track_id) {
                            cx.notify();
                        }
                    });
                })
            },
            on_select_take: {
                let this = cx.entity().clone();
                std::sync::Arc::new(move |(track_id, take_id), _window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        if this.state.set_active_take(track_id, take_id) {
                            // Muting is what makes a take inactive, so the
                            // engine has to hear about it like any other clip
                            // change.
                            this.mark_project_changed(cx);
                            cx.notify();
                        }
                    });
                })
            },
            on_delete_take: {
                let this = cx.entity().clone();
                std::sync::Arc::new(move |(track_id, take_id), _window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        if this.state.delete_take(track_id, take_id) {
                            this.mark_project_changed(cx);
                            cx.notify();
                        }
                    });
                })
            },
            on_open_instrument: self.on_open_track_instrument.clone(),
        };

        // Shared with the cached lane views, so the row geometry and the
        // gesture snapshot are derived once per frame rather than once here and
        // again inside every lane that has to rebuild.
        let row_layout = std::rc::Rc::new(row_layout);
        let lane_gesture = std::rc::Rc::new(
            crate::components::timeline::timeline_state::TimelineGestureContext::from_state(
                &self.state,
            ),
        );
        self.frame_lane_ctx = Some(std::rc::Rc::new(
            crate::components::timeline::track_lane_view::LaneFrameContext {
                gesture: lane_gesture.clone(),
                row_layout: row_layout.clone(),
                on_select_track: on_select_track.clone(),
                on_select_clip: on_select_clip.clone(),
                on_add_clip: on_add_clip.clone(),
                on_track_context_menu: on_track_context_menu.clone(),
                on_clip_context_menu: on_clip_context_menu.clone(),
                on_open_editor: self.on_open_editor.clone(),
                on_range_start: Some(on_range_start.clone()),
                on_erase_start: Some(on_erase_start.clone()),
                on_erase_clip: Some(on_erase_clip.clone()),
                on_cut_clip: Some(on_cut_clip.clone()),
                on_audio_clip_process_preview: on_audio_clip_process_preview.clone(),
                on_audio_clip_process_commit: on_audio_clip_process_commit.clone(),
            },
        ));

        let state = &self.state;
        let tempo_h = state.tempo_track_height();
        let ts_h = state.time_signature_track_height();
        let marker_h = state.marker_track_height();
        let region_h = state.region_track_height();
        let chord_h = state.chord_track_height();
        let content_top = state.arrangement_content_top();
        // Live pen-draw ghost clip (built before the chain to keep the borrow of
        // `self.pen_clip_draw` separate from the render closures).
        let pen_preview_overlay = self
            .pen_clip_draw
            .as_ref()
            .and_then(|preview| pen_clip_draw_overlay(preview, state));
        let file_drop_overlay = self
            .file_drop_hint
            .as_ref()
            .and_then(|hint| file_drop_hint_overlay(hint, state));
        let clip_clone_overlay = self
            .clip_clone_hint
            .as_ref()
            .and_then(|hint| clip_clone_hint_overlay(hint, state));
        let chord_drop_overlay =
            crate::components::timeline::chord_track::chord_drop_clip_overlay(state);
        let plugin_drop_overlay = self
            .plugin_drop_hint
            .as_ref()
            .and_then(|hint| plugin_drop::plugin_drop_hint_overlay(hint, state));
        let on_zoom_in_btn = on_zoom_in.clone();
        let on_zoom_out_btn = on_zoom_out.clone();

        // ── Scrollbar geometry ──────────────────────────────────────────
        // Computed once per render against the live window size. Both
        // tracks (visible bar) are 8 px wide and sit at the right/bottom
        // edges of the lane area. Clicking the track jumps the scroll
        // position to that point — gives a functional scrollbar without
        // needing a stateful drag.
        let content_w = self.timeline_content_width();
        let content_h = row_layout.total_height.max(1.0);
        let lane_view_h = viewport_h.max(DEFAULT_TRACK_HEIGHT);
        let lane_view_w = viewport_w.max(1.0);
        let toolbar_default = (16.0, (content_top + lane_view_h - 48.0).max(16.0));
        let toolbar_position = self.floating_toolbar_position.unwrap_or(toolbar_default);
        let on_toolbar_drag_start: std::sync::Arc<
            dyn Fn(&(f32, f32), &mut Window, &mut gpui::App) + 'static,
        > = {
            let target = cx.entity().clone();
            std::sync::Arc::new(move |point, _window, cx| {
                let point = *point;
                let _ = target.update(cx, |this, cx| {
                    this.floating_toolbar_drag_anchor = Some((
                        gpui::point(gpui::px(point.0), gpui::px(point.1)),
                        toolbar_position,
                    ));
                    cx.notify();
                });
            })
        };

        // ── Drag/drop import wiring ─────────────────────────────────────
        // Track the mouse position throughout an external file drag so that
        // when `on_drop` fires we can resolve the drop coordinates.
        let on_drag_track = cx.listener(
            |this, event: &gpui::DragMoveEvent<ExternalPaths>, _window, cx| {
                this.last_drag_position = Some(event.event.position);
                this.file_drop_hint = Some(FileDropHint {
                    position: event.event.position,
                    label: file_drop_hint_label(event.drag(cx).paths()),
                });
                this.clip_clone_hint = None;
                cx.notify();
            },
        );

        let on_files_dropped = cx.listener(|this, paths: &ExternalPaths, window, cx| {
            // The final OLE Drop callback is authoritative. A platform drag can
            // be released between two DragOver notifications, so relying only
            // on `last_drag_position` can place the imported clip at the
            // previous frame's coordinates (especially with a scaled Windows
            // client area). GPUI has already normalized the drop point to the
            // same logical window space used by mouse events.
            this.last_drag_position = Some(window.mouse_position());
            let mut any_imported = false;
            // Multi-file drops: the first file lands at the cursor; subsequent
            // files always land on a brand-new track (forced via y past the end).
            let mut force_new_track = false;
            for path in paths.paths().iter() {
                let imported =
                    this.import_midi_path_at_last_drag(path, force_new_track, window, cx)
                        || this.import_audio_path_at_last_drag(path, force_new_track, window, cx)
                        || this.import_video_path_at_last_drag(path, force_new_track, window, cx);
                any_imported |= imported;
                force_new_track |= imported;
            }
            let had_hint = this.file_drop_hint.take().is_some();
            if any_imported {
                this.last_drag_position = None;
            }
            if any_imported || had_hint {
                cx.notify();
            }
        });

        let on_browser_drag_track = cx.listener(
            |this, event: &gpui::DragMoveEvent<BrowserDragItem>, _window, cx| {
                this.last_drag_position = Some(event.event.position);
                this.file_drop_hint = Some(FileDropHint {
                    position: event.event.position,
                    label: file_drop_hint_label(std::slice::from_ref(&event.drag(cx).path)),
                });
                this.clip_clone_hint = None;
                cx.notify();
            },
        );

        let on_browser_file_dropped = cx.listener(|this, item: &BrowserDragItem, window, cx| {
            this.last_drag_position = Some(window.mouse_position());
            let had_hint = this.file_drop_hint.take().is_some();
            let imported = this.import_midi_path_at_last_drag(&item.path, false, window, cx)
                || this.import_audio_path_at_last_drag(&item.path, false, window, cx)
                || this.import_video_path_at_last_drag(&item.path, false, window, cx)
                || this.drop_plugin_preset_at_last_drag(&item.path, window, cx);
            if imported {
                this.last_drag_position = None;
            }
            if imported || had_hint {
                cx.notify();
            }
        });

        // Plug-in from the Browser: a header takes it, the timeline makes a
        // new track for it. The hint and the drop resolve through the same
        // function, so the hint never promises something the drop won't do.
        let on_plugin_drag_move = cx.listener(
            |this,
             event: &gpui::DragMoveEvent<crate::components::plugin_picker::PluginDragItem>,
             _window,
             cx| {
                // Drag moves arrive while the pointer is anywhere in the
                // window; only the arrangement shows a target.
                if !event.bounds.contains(&event.event.position) {
                    if this.plugin_drop_hint.take().is_some() {
                        cx.notify();
                    }
                    return;
                }
                let item = event.drag(cx).clone();
                let target = this.resolve_context_target_from_window_point(event.event.position);
                let hint = plugin_drop::PluginDropHint {
                    target: plugin_drop::resolve_plugin_drop(
                        &this.state,
                        &target,
                        item.kind == SpherePluginHost::PluginKind::Instrument,
                    ),
                    plugin_name: item.label,
                };
                if this.plugin_drop_hint.as_ref().map(|h| &h.target) != Some(&hint.target) {
                    this.plugin_drop_hint = Some(hint);
                    cx.notify();
                }
            },
        );
        let on_plugin_drag_dropped = cx.listener(
            |this, item: &crate::components::plugin_picker::PluginDragItem, window, cx| {
                this.plugin_drop_hint = None;
                let target = this.resolve_context_target_from_window_point(window.mouse_position());
                let drop = plugin_drop::resolve_plugin_drop(
                    &this.state,
                    &target,
                    item.kind == SpherePluginHost::PluginKind::Instrument,
                );
                if !matches!(drop, PluginDropTarget::Refused { .. }) {
                    if let Some(callback) = this.on_plugin_drag_drop.as_ref() {
                        callback(item, &drop, window, cx);
                    }
                }
                cx.notify();
            },
        );

        let on_clip_drag_move = cx.listener(
            |this, event: &gpui::DragMoveEvent<ClipDragItem>, window, cx| {
                let drag = event.drag(cx).clone();
                let bypass_snap = event.event.modifiers.shift;
                this.last_drag_position = Some(event.event.position);
                this.file_drop_hint = None;
                if this.clip_clone_drag_id.as_deref() == Some(drag.clip_id.as_str()) {
                    let origin = *this.clip_drag_origin.get_or_insert(event.event.position);
                    let (target_index, start_beat) = this.resolve_clip_drag_target_with_bypass(
                        &drag,
                        origin,
                        event.event.position,
                        bypass_snap,
                    );
                    this.clip_drag_target_track_index = Some(target_index);
                    this.clip_clone_hint = Some(ClipCloneHint {
                        clip_id: drag.clip_id.clone(),
                        target_track_index: target_index,
                        start_beat,
                    });
                } else {
                    this.clip_clone_hint = None;
                    this.move_dragged_clip_to_position_with_bypass(
                        &drag,
                        event.event.position,
                        window,
                        bypass_snap,
                    );
                }
                cx.notify();
            },
        );

        let on_clip_dropped = cx.listener(|this, drag: &ClipDragItem, _window, cx| {
            let target_index = this.clip_drag_target_track_index;
            if let Some(target_track_id) = target_index
                .and_then(|index| this.state.tracks.get(index))
                .map(|track| track.id.clone())
            {
                if this.clip_clone_drag_id.as_deref() == Some(drag.clip_id.as_str()) {
                    // Commit the exact preview target, including live Shift snap
                    // bypass, rather than resolving a second time with snap on.
                    let start_beat = this
                        .clip_clone_hint
                        .as_ref()
                        .filter(|hint| hint.clip_id == drag.clip_id)
                        .map(|hint| hint.start_beat)
                        .unwrap_or_else(|| {
                            let origin = this
                                .clip_drag_origin
                                .unwrap_or_else(|| this.last_drag_position.unwrap_or_default());
                            let position = this.last_drag_position.unwrap_or(origin);
                            this.resolve_clip_drag_target(drag, origin, position).1
                        });
                    this.create_clip_clone_group_at(
                        &drag.clip_id,
                        &target_track_id,
                        start_beat,
                        cx,
                    );
                } else {
                    let drag_ids = this.clip_drag_selection_ids(&drag.clip_id);
                    let resolved_target_index = target_index.unwrap_or_else(|| {
                        this.state
                            .tracks
                            .iter()
                            .position(|track| track.id == target_track_id)
                            .unwrap_or(0)
                    });
                    let source_index = this
                        .state
                        .tracks
                        .iter()
                        .position(|track| track.id == drag.source_track_id)
                        .unwrap_or(resolved_target_index);
                    let track_delta = resolved_target_index as isize - source_index as isize;
                    let max_index = this.state.tracks.len().saturating_sub(1) as isize;

                    for clip_id in &drag_ids {
                        let Some((track_index, current_start)) = this
                            .state
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
                        let target_track_id = this
                            .state
                            .tracks
                            .get((track_index as isize + track_delta).clamp(0, max_index) as usize)
                            .map(|track| track.id.clone())
                            .unwrap_or_else(|| target_track_id.clone());
                        this.state
                            .move_clip_to_track(clip_id, &target_track_id, current_start);
                    }
                    this.restore_clip_drag_selection(
                        &drag.clip_id,
                        drag_ids,
                        Some(target_track_id),
                    );
                    this.mark_project_changed(cx);
                }
            }
            this.clip_drag_origin = None;
            this.clip_drag_target_track_index = None;
            this.clip_clone_drag_id = None;
            this.clip_clone_hint = None;
            this.last_drag_position = None;
            this.file_drop_hint = None;
            cx.notify();
        });

        // Clip edge-resize: live-mutate the clip bounds on every drag move (no
        // dirty), then commit once on drop. `resize_clip` snaps internally.
        let on_clip_resize_move = cx.listener(
            |this, event: &gpui::DragMoveEvent<ClipResizeDrag>, _window, cx| {
                let drag = event.drag(cx).clone();
                // Capture the pre-gesture clip once, before the first mutation.
                // This is what the drop turns into the undo step's `previous`.
                if this
                    .clip_resize_origin
                    .as_ref()
                    .is_none_or(|origin| origin.clip.id != drag.clip_id)
                {
                    this.clip_resize_origin = ClipSnapshot::capture(&this.state, &drag.clip_id);
                }
                let beat = this.beat_from_window_x(event.event.position.x.into());
                this.state.resize_clip_with_bypass(
                    &drag.clip_id,
                    drag.edge,
                    beat,
                    event.event.modifiers.shift,
                );
                cx.notify();
            },
        );
        let on_clip_resize_drop = cx.listener(|this, drag: &ClipResizeDrag, _window, cx| {
            // No drag-move means nothing was resized, so there is no undo step.
            let origin = this
                .clip_resize_origin
                .take()
                .filter(|origin| origin.clip.id == drag.clip_id);
            if let (Some(previous), Some(next)) =
                (origin, ClipSnapshot::capture(&this.state, &drag.clip_id))
            {
                if previous.clip != next.clip {
                    this.record_executed_command(EditCommand::UpdateClip { previous, next }, cx);
                    this.mark_project_changed(cx);
                }
            }
            cx.notify();
        });

        let on_song_text_drag_move = cx.listener(
            |this, event: &gpui::DragMoveEvent<SongTextDragSession>, _window, cx| {
                if this.song_text_drag_cancelled {
                    return;
                }
                let drag = event.drag(cx).clone();
                let pointer_beat = this.beat_from_window_x(event.event.position.x.into()) as f64;
                let pixels_per_beat = this.state.viewport.pixels_per_beat.max(1.0) as f64;
                let raw_anchor =
                    (pointer_beat - drag.pointer_offset_x as f64 / pixels_per_beat).max(0.0);
                let snapped_anchor = this
                    .state
                    .snap_beats_with_bypass(raw_anchor as f32, event.event.modifiers.shift)
                    as f64;
                this.song_text_drag_preview = Some(SongTextDragPreview {
                    positions: song_text_drag_positions(
                        &drag.anchor_event_id,
                        &drag.original_positions,
                        snapped_anchor,
                    ),
                });
                cx.notify();
            },
        );
        let on_song_text_drag_drop =
            cx.listener(|this, drag: &SongTextDragSession, _window, cx| {
                if this.song_text_drag_cancelled {
                    this.song_text_drag_preview = None;
                    return;
                }
                let Some(preview) = this.song_text_drag_preview.take() else {
                    return;
                };
                let previous: Vec<_> = drag
                    .original_positions
                    .iter()
                    .filter_map(|(id, _)| this.state.song_text_event(id).cloned())
                    .collect();
                let positions: std::collections::HashMap<_, _> =
                    preview.positions.into_iter().collect();
                let next: Vec<_> = previous
                    .iter()
                    .cloned()
                    .map(|mut event| {
                        if let Some(beat) = positions.get(&event.id) {
                            event.beat = *beat;
                        }
                        event
                    })
                    .collect();
                if previous != next {
                    this.run_metadata_edit_command(
                        EditCommand::SetSongTextEvents {
                            label: "Move Song Text",
                            previous,
                            next,
                        },
                        cx,
                    );
                } else {
                    cx.notify();
                }
            });

        let on_global_lane_resize_move = cx.listener(
            |this, event: &gpui::DragMoveEvent<GlobalLaneResizeDrag>, _window, cx| {
                let y: f32 = event.event.position.y.into();
                if this.state.ensure_global_lane_resize_from_arm(y) {
                    this.state.update_global_lane_resize(y);
                    cx.notify();
                }
            },
        );
        let on_global_lane_resize_drop =
            cx.listener(|this, _drag: &GlobalLaneResizeDrag, _window, cx| {
                this.state.clear_global_lane_resize_arm();
                if let Some((prev, next)) = this.state.finish_global_lane_resize() {
                    this.record_executed_command(
                        EditCommand::SetGlobalLaneHeights { prev, next },
                        cx,
                    );
                }
                cx.notify();
            });
        let on_global_lane_resize_arm: GlobalLaneResizeArmCb = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |(kind, y): &(GlobalLaneKind, f32), _window, cx| {
                let (kind, y) = (*kind, *y);
                let _ = this.update(cx, |this, cx| {
                    this.state.arm_global_lane_resize(kind, y);
                    cx.notify();
                });
            })
        };
        let on_global_lane_resize_reset: GlobalLaneResizeResetCb = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |kind: &GlobalLaneKind, _window, cx| {
                let kind = *kind;
                let _ = this.update(cx, |this, cx| {
                    if let Some((prev, next)) = this.state.reset_global_lane_height(kind) {
                        this.record_executed_command(
                            EditCommand::SetGlobalLaneHeights { prev, next },
                            cx,
                        );
                    }
                    cx.notify();
                });
            })
        };

        let on_track_height_resize_move = cx.listener(
            |this, event: &gpui::DragMoveEvent<TrackHeightResizeDrag>, _window, cx| {
                let y: f32 = event.event.position.y.into();
                if this.state.ensure_track_height_resize_from_arm(y) {
                    this.state.update_track_height_resize(y);
                    cx.notify();
                }
            },
        );
        let on_track_height_resize_drop =
            cx.listener(|this, _drag: &TrackHeightResizeDrag, _window, cx| {
                this.state.clear_track_height_resize_arm();
                if let Some((prev, next)) = this.state.finish_track_height_resize() {
                    this.record_executed_command(EditCommand::SetTrackHeights { prev, next }, cx);
                    this.mark_project_changed(cx);
                }
                cx.notify();
            });

        let on_track_height_resize_arm: std::sync::Arc<
            dyn Fn(&(String, f32, bool, bool), &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(
                move |(track_id, y, shift, alt): &(String, f32, bool, bool), _window, cx| {
                    let _ = this.update(cx, |this, cx| {
                        this.state
                            .arm_track_height_resize(track_id, *y, *shift, *alt);
                        cx.notify();
                    });
                },
            )
        };
        let on_track_height_resize_reset: std::sync::Arc<
            dyn Fn(&String, &mut Window, &mut gpui::App) + 'static,
        > = {
            let this = cx.entity().clone();
            std::sync::Arc::new(move |track_id: &String, _window, cx| {
                let _ = this.update(cx, |this, cx| {
                    if this.state.reset_track_row_height(track_id) {
                        this.mark_project_changed(cx);
                        cx.notify();
                    }
                });
            })
        };

        let on_track_drag_move = cx.listener(
            |this, event: &gpui::DragMoveEvent<TrackDragItem>, _window, cx| {
                let drag = event.drag(cx).clone();
                let y = this.track_area_y_from_window(event.event.position);
                if this.state.dragging_track_id.as_deref() != Some(drag.track_id.as_str()) {
                    this.state
                        .begin_track_drag(&drag.track_id, drag.origin_index, y);
                }
                this.state.update_track_drag(y);
                cx.notify();
            },
        );

        let on_track_dropped = cx.listener(|this, drag: &TrackDragItem, _window, cx| {
            let dragged_parent_group = this
                .state
                .find_track(&drag.track_id)
                .and_then(|track| track.parent_group_id.clone());
            let hovered_track = this
                .state
                .track_index_at_y(this.state.drag_current_y)
                .and_then(|index| this.state.tracks.get(index))
                .map(|track| (track.id.clone(), track.parent_group_id.clone()));
            let hovered_group = (!drag.is_group)
                .then(|| {
                    hovered_track.as_ref().and_then(|(hovered_id, _)| {
                        this.state
                            .find_track(hovered_id)
                            .filter(|track| {
                                track.track_type == TrackType::Group && track.id != drag.track_id
                            })
                            .map(|track| track.id.clone())
                    })
                })
                .flatten();
            if let Some(group_id) = hovered_group {
                if this.state.assign_track_to_group(&drag.track_id, &group_id) {
                    this.mark_project_changed(cx);
                    cx.notify();
                }
                return;
            }
            let target_index = this
                .state
                .drag_target_index
                .unwrap_or(drag.origin_index)
                .clamp(0, this.state.tracks.len());
            let remains_inside_group = dragged_parent_group.as_ref().is_some_and(|group_id| {
                hovered_track
                    .as_ref()
                    .is_some_and(|(hovered_id, parent_id)| {
                        hovered_id == group_id || parent_id.as_ref() == Some(group_id)
                    })
            });
            if !remains_inside_group {
                this.state.remove_track_from_group(&drag.track_id);
            }
            this.state.reorder_track(&drag.track_id, target_index);
            this.mark_project_changed(cx);
            cx.notify();
        });

        let on_middle_pan_start = cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
            this.pan_last_position = Some(event.position);
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
        });

        let on_middle_pan_move = cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
            if event.pressed_button != Some(gpui::MouseButton::Middle) {
                this.pan_last_position = None;
                return;
            }

            let Some(previous) = this.pan_last_position else {
                this.pan_last_position = Some(event.position);
                return;
            };

            let dx: f32 = (event.position.x - previous.x).into();
            let dy: f32 = (event.position.y - previous.y).into();
            let (max_x, max_y) = this.max_scroll_offsets(window);
            let next_x = this.state.viewport.scroll_x - dx;
            let next_y = this.state.viewport.scroll_y - dy;
            this.state
                .set_scroll_immediate(next_x, next_y, max_x, max_y);
            this.pan_last_position = Some(event.position);
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
        });

        let on_middle_pan_end = cx.listener(|this, _event: &gpui::MouseUpEvent, _window, cx| {
            this.pan_last_position = None;
            cx.notify();
        });

        let on_middle_pan_end_out =
            cx.listener(|this, _event: &gpui::MouseUpEvent, _window, cx| {
                this.pan_last_position = None;
                cx.notify();
            });

        let on_ctrl_wheel_zoom = cx.listener(|this, event: &gpui::ScrollWheelEvent, window, cx| {
            let delta = match event.delta {
                ScrollDelta::Pixels(p) => {
                    let x: f32 = p.x.into();
                    let y: f32 = p.y.into();
                    (x, y)
                }
                ScrollDelta::Lines(p) => (p.x * 36.0, p.y * 36.0),
            };

            if !(event.modifiers.control || event.modifiers.platform) {
                let (max_x, max_y) = this.max_scroll_offsets(window);
                let (scroll_x, scroll_y) = if event.modifiers.shift {
                    let horizontal = if delta.1.abs() > 0.01 {
                        delta.1
                    } else {
                        delta.0
                    };
                    (horizontal, 0.0)
                } else {
                    (delta.0, delta.1)
                };
                // GPUI wheel deltas describe finger/wheel movement, whereas
                // the timeline offsets describe the content origin. Invert at
                // this boundary so wheel/trackpad motion follows the direction
                // users see, matching the middle-button grab-pan behavior.
                // Preferences → Editing → Natural Scroll flips that mapping.
                let natural = cx
                    .try_global::<crate::settings::GlobalSettingsModel>()
                    .map(|g| g.0.read(cx).current.editing.mouse.natural_scroll)
                    .unwrap_or(false);
                let (next_x, next_y) = if natural {
                    (
                        this.state.viewport.scroll_x + scroll_x,
                        this.state.viewport.scroll_y + scroll_y,
                    )
                } else {
                    (
                        this.state.viewport.scroll_x - scroll_x,
                        this.state.viewport.scroll_y - scroll_y,
                    )
                };
                this.state
                    .set_scroll_immediate(next_x, next_y, max_x, max_y);
                if scroll_x.abs() > 0.5 || scroll_y.abs() > 0.5 {
                    this.state.note_user_scrolled();
                }
                window.prevent_default();
                cx.stop_propagation();
                cx.notify();
                return;
            }

            window.prevent_default();
            cx.stop_propagation();

            if delta.1.abs() < 0.01 {
                return;
            }

            let x: f32 = event.position.x.into();
            let anchor = this.state.lane_x_from_window_x(x).max(0.0);
            let factor = wheel_zoom_factor(delta.1);
            this.state.zoom_by(factor, anchor);
            let (max_x, max_y) = this.max_scroll_offsets(window);
            this.state.clamp_scroll(max_x, max_y);
            cx.notify();
        });

        // Trackpad pinch-to-zoom (macOS Magnify, Windows Direct Manipulation,
        // Linux X11/Wayland gesture). GPUI already emits PinchEvent on all
        // three; the timeline previously only handled Ctrl/Cmd + wheel.
        let on_pinch_zoom = cx.listener(|this, event: &PinchEvent, window, cx| {
            let factor = pinch_zoom_factor(event.delta);
            if (factor - 1.0).abs() < 0.0001 {
                return;
            }
            window.prevent_default();
            cx.stop_propagation();
            let x: f32 = event.position.x.into();
            let anchor = this.state.lane_x_from_window_x(x).max(0.0);
            this.state.zoom_by(factor, anchor);
            let (max_x, max_y) = this.max_scroll_offsets(window);
            this.state.clamp_scroll(max_x, max_y);
            cx.notify();
        });

        let on_arrangement_context_menu = on_timeline_context.clone().map(|cb| {
            cx.listener(
                move |this, event: &gpui::MouseDownEvent, window: &mut gpui::Window, cx| {
                    let x: f32 = event.position.x.into();
                    let y: f32 = event.position.y.into();
                    let target = this.resolve_context_target_from_window_point(event.position);
                    cb(&(target, x, y), window, cx);
                },
            )
        });

        let _phase = {
            drop(phase);
            crate::perf::PerfScope::enter("Timeline:tree")
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .bg(Colors::surface_base())
            .border_l(px(1.0))
            .border_r(px(1.0))
            .border_color(Colors::border_subtle())
            .relative()
            .capture_key_down(
                cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                    if event.keystroke.key.as_str() == "escape"
                        && (this.range_select_drag.is_some()
                            || this.marker_drag.is_some()
                            || this.pen_clip_draw.is_some()
                            || this.erase_clip_drag.is_some()
                            || this.automation_drag.is_some()
                            || this.automation_marquee.is_some()
                            || this.song_text_drag_preview.is_some()
                            || this.pan_last_position.is_some()
                            || this.state.track_height_resize.is_some()
                            || this.state.track_height_resize_arm.is_some())
                    {
                        cx.stop_propagation();
                        this.cancel_active_gesture(cx);
                    }
                }),
            )
            .on_drag_move::<ExternalPaths>(on_drag_track)
            .on_drop::<ExternalPaths>(on_files_dropped)
            .on_drag_move::<BrowserDragItem>(on_browser_drag_track)
            .on_drop::<BrowserDragItem>(on_browser_file_dropped)
            .on_drag_move::<crate::components::plugin_picker::PluginDragItem>(on_plugin_drag_move)
            .on_drop::<crate::components::plugin_picker::PluginDragItem>(on_plugin_drag_dropped)
            .on_drag_move::<ClipDragItem>(on_clip_drag_move)
            .on_drop::<ClipDragItem>(on_clip_dropped)
            .on_drag_move::<ClipResizeDrag>(on_clip_resize_move)
            .on_drop::<ClipResizeDrag>(on_clip_resize_drop)
            .on_drag_move::<SongTextDragSession>(on_song_text_drag_move)
            .on_drop::<SongTextDragSession>(on_song_text_drag_drop)
            .on_drag_move::<TrackHeightResizeDrag>(on_track_height_resize_move)
            .on_drop::<TrackHeightResizeDrag>(on_track_height_resize_drop)
            .on_drag_move::<GlobalLaneResizeDrag>(on_global_lane_resize_move)
            .on_drop::<GlobalLaneResizeDrag>(on_global_lane_resize_drop)
            // Regions are dragged on the ruler, but the drop lands wherever the
            // pointer is released — take it at the surface, like the resize
            // gestures, so the undo entry is always recorded.
            .on_drop::<TimelineRegionDrag>(on_region_drag_drop)
            .on_drop::<ChordEventDrag>(on_chord_drag_drop)
            .on_drag_move::<TrackDragItem>(on_track_drag_move)
            .on_drop::<TrackDragItem>(on_track_dropped)
            .on_mouse_down(gpui::MouseButton::Middle, on_middle_pan_start)
            .when_some(on_arrangement_context_menu, |this, cb| {
                this.on_mouse_down(gpui::MouseButton::Right, cb)
            })
            .on_mouse_move(on_middle_pan_move)
            .on_mouse_move(on_edit_mouse_move)
            .on_mouse_up(gpui::MouseButton::Middle, on_middle_pan_end)
            .on_mouse_up_out(gpui::MouseButton::Middle, on_middle_pan_end_out)
            .on_mouse_up(gpui::MouseButton::Left, on_pen_mouse_up)
            .on_mouse_up_out(gpui::MouseButton::Left, on_pen_mouse_up_out)
            .on_mouse_up(
                gpui::MouseButton::Right,
                cx.listener(|this, _ev, _w, cx| {
                    this.log_input_state("mouse-up-right");
                    if this.erase_clip_drag.is_some() {
                        this.finish_erase_clip_drag(cx);
                    }
                    this.reset_input_state();
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Right,
                cx.listener(|this, _ev, _w, cx| {
                    this.log_input_state("mouse-up-right-out");
                    if this.erase_clip_drag.is_some() {
                        this.finish_erase_clip_drag(cx);
                    }
                    this.reset_input_state();
                    cx.notify();
                }),
            )
            .on_scroll_wheel(on_ctrl_wheel_zoom)
            .on_pinch(on_pinch_zoom)
            // 1. Timeline Ruler
            .child(timeline_ruler(
                state,
                on_add_track.clone(),
                on_toggle_snap.clone(),
                on_cycle_grid.clone(),
                on_clear_all_mutes,
                on_clear_all_solos,
                on_seek.clone(),
                on_region_drag.clone(),
                on_loop_drag.clone(),
                on_ruler_context.clone(),
                on_playhead_scrub_begin,
                on_playhead_scrub_end,
                self.ruler_gesture.clone(),
                self.lane_origin_probe.clone(),
            ))
            // 1b. Conductor lanes, in `visible_global_lanes()` order: structure
            // (regions, markers) closest to the ruler, then tempo and meter.
            .when(state.show_region_track, |this| {
                this.child(region_track_lane(
                    state,
                    region_h,
                    Some(on_region_down.clone()),
                    on_region_context.clone(),
                    Some(on_region_drag.clone()),
                    Some(on_region_add.clone()),
                    on_region_header_menu.clone(),
                    Some(on_region_hide.clone()),
                    Some(on_region_toggle_collapsed.clone()),
                    Some(on_global_lane_resize_arm.clone()),
                    Some(on_global_lane_resize_reset.clone()),
                ))
            })
            .when(state.show_marker_track, |this| {
                this.child(marker_track_lane(
                    state,
                    marker_h,
                    Some(on_marker_down.clone()),
                    on_marker_context.clone(),
                    Some(on_marker_add.clone()),
                    on_marker_header_menu.clone(),
                    Some(on_marker_hide.clone()),
                    Some(on_marker_toggle_collapsed.clone()),
                    Some(on_global_lane_resize_arm.clone()),
                    Some(on_global_lane_resize_reset.clone()),
                ))
            })
            // Order must match `visible_global_lanes()`: Chord sits between
            // the structure lanes and the conductor lanes.
            .when(state.show_chord_track, |this| {
                this.child(chord_track_lane(
                    state,
                    chord_h,
                    Some(on_chord_down.clone()),
                    on_chord_context.clone(),
                    Some(on_chord_drag.clone()),
                    Some(on_chord_open_generator.clone()),
                    on_chord_header_menu.clone(),
                    Some(on_chord_hide.clone()),
                    Some(on_chord_toggle_collapsed.clone()),
                    Some(on_global_lane_resize_arm.clone()),
                    Some(on_global_lane_resize_reset.clone()),
                ))
            })
            .when(state.show_tempo_track, |this| {
                this.child(tempo_track_lane(
                    state,
                    tempo_h,
                    Some(on_tempo_down.clone()),
                    on_tempo_context.clone(),
                    Some(on_tempo_add.clone()),
                    on_tempo_header_menu.clone(),
                    Some(on_tempo_hide.clone()),
                    Some(on_tempo_toggle_collapsed.clone()),
                    Some(on_global_lane_resize_arm.clone()),
                    Some(on_global_lane_resize_reset.clone()),
                ))
            })
            .when(state.show_time_signature_track, |this| {
                this.child(time_signature_track_lane(
                    state,
                    ts_h,
                    Some(on_ts_down.clone()),
                    on_ts_context.clone(),
                    Some(on_ts_add.clone()),
                    on_ts_header_menu.clone(),
                    Some(on_ts_hide.clone()),
                    Some(on_ts_toggle_collapsed.clone()),
                    Some(on_global_lane_resize_arm.clone()),
                    Some(on_global_lane_resize_reset.clone()),
                ))
            })
            .when(state.show_song_text_track, |this| {
                this.child(song_text_track_lane(
                    state,
                    self.song_text_drag_preview.as_ref(),
                    on_song_text_marker_down,
                    on_song_text_marker_context,
                    on_song_text_empty_seek,
                    Some(on_song_text_hide.clone()),
                    Some(on_global_lane_resize_arm.clone()),
                    Some(on_global_lane_resize_reset.clone()),
                ))
            })
            // 2. Track List Scroll Area
            .child(div().flex_1().min_h_0().relative().child(track_list(
                state,
                &row_layout,
                &self.track_meters,
                header_callbacks.clone(),
                on_track_height_resize_arm.clone(),
                on_track_height_resize_reset.clone(),
                on_select_track.clone(),
                on_select_clip.clone(),
                on_add_clip.clone(),
                on_track_context_menu.clone(),
                on_clip_context_menu.clone(),
                self.on_open_editor.clone(),
                Some(on_range_start.clone()),
                Some(on_erase_start.clone()),
                Some(on_erase_clip.clone()),
                Some(on_cut_clip.clone()),
                Some(&self.erase_preview_ids),
                on_audio_clip_process_preview.clone(),
                on_audio_clip_process_commit.clone(),
                Some(on_automation_down.clone()),
                Some(on_automation_lane_action.clone()),
                Some(on_automation_hover.clone()),
                Some(on_automation_delete),
                on_automation_control.clone(),
                self.automation_marquee.as_ref(),
                self.automation_hover.as_ref(),
                match self.arrangement_surface.as_ref() {
                    Some(surface) => gpui::AnyView::from(surface.clone())
                        .cached(
                            crate::components::timeline::timeline_surface::TimelineSurfaceView::cached_style(),
                        )
                        .into_any_element(),
                    None => self.render_arrangement_surface(),
                },
                &lane_gesture,
                &self.track_lanes,
            )))
            .children(timeline_marker_region_overlay(state).map(|overlay| {
                div()
                    .absolute()
                    .left(px(HEADER_WIDTH))
                    .right_0()
                    .top(px(content_top))
                    .bottom_0()
                    .overflow_hidden()
                    .child(overlay)
            }))
            // 3. Playhead Overlay (frontmost timeline pass)
            // Render after ruler + content so grid/ruler/content never cover it.
            // Split into:
            // - head overlay (ruler strip only)
            // - body overlay (content strip only)
            // 2b. Arrangement range-selection overlay (UI-only). Drawn above the
            // lane content but below the playhead/tools so it never hides the
            // playhead. Follows zoom/scroll via the same lane coordinate space.
            .children(arrangement_range_overlay(state).map(|overlay| {
                div()
                    .absolute()
                    .left(px(HEADER_WIDTH))
                    .right_0()
                    .top(px(content_top))
                    .bottom_0()
                    .overflow_hidden()
                    .child(overlay)
            }))
            // Live pen-draw MIDI clip ghost preview (same lane coordinate space
            // as the arrangement overlay; above content, below the playhead).
            .children(pen_preview_overlay.map(|overlay| {
                div()
                    .absolute()
                    .left(px(HEADER_WIDTH))
                    .right_0()
                    .top(px(content_top))
                    .bottom_0()
                    .overflow_hidden()
                    .child(overlay)
            }))
            // External/browser file-drop hint. Drawn above lane content and below
            // playhead/tools; it is UI-only and follows the last GPUI drag move.
            .children(file_drop_overlay.map(|overlay| {
                div()
                    .absolute()
                    .left(px(HEADER_WIDTH))
                    .right_0()
                    .top(px(content_top))
                    .bottom_0()
                    .overflow_hidden()
                    .child(overlay)
            }))
            // Chord Generator drop ghost (a progression becoming a MIDI clip).
            .children(chord_drop_overlay.map(|overlay| {
                div()
                    .absolute()
                    .left(px(HEADER_WIDTH))
                    .right_0()
                    .top(px(content_top))
                    .bottom_0()
                    .overflow_hidden()
                    .child(overlay)
            }))
            // Plug-in drop hint. Full width, headers included: the header is
            // one of the two places a plug-in can go.
            .children(plugin_drop_overlay.map(|overlay| {
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(content_top))
                    .bottom_0()
                    .overflow_hidden()
                    .child(overlay)
            }))
            // Alt-drag clone ghost. Like the MIDI pen/file-drop previews, this
            // stays transient until the user releases the pointer.
            .children(clip_clone_overlay.map(|overlay| {
                div()
                    .absolute()
                    .left(px(HEADER_WIDTH))
                    .right_0()
                    .top(px(content_top))
                    .bottom_0()
                    .overflow_hidden()
                    .child(overlay)
            }))
            // The playhead is its own entity: it moves at the display rate
            // while the transport runs, and drawing it inline meant every one of
            // those frames rebuilt the whole arrangement behind it. Its head and
            // body are both in there, so the line stays continuous through the
            // conductor lanes rather than restarting below them.
            .children(playhead_overlay)
            // 4. Floating Tools Bar (above playhead)
            .child(
                div()
                    .absolute()
                    .left(px(toolbar_position.0))
                    .top(px(toolbar_position.1))
                    .child(floating_tools_bar(
                        state.active_tool,
                        on_select_tool.clone(),
                        on_toolbar_drag_start,
                    )),
            )
            // 5. Vertical scrollbar (right edge, over the lane area)
            .child(vertical_scrollbar(
                cx,
                state.viewport.scroll_y,
                content_h,
                lane_view_h,
                scroll_max_y,
                content_top,
            ))
            // 6. Horizontal scrollbar (bottom edge, over the lane area)
            .child(horizontal_scrollbar(
                cx,
                state.viewport.scroll_x,
                content_w,
                lane_view_w,
                scroll_max_x,
            ))
            // 7. Zoom Controls
            .child(
                div()
                    .absolute()
                    .bottom(px(16.0))
                    .right(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(crate::theme::radius::PILL))
                    .border(px(1.0))
                    .border_color(Colors::border_default())
                    .bg(Colors::surface_panel_alt())
                    .shadow_xl()
                    // Zoom Out Button
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(24.0))
                            .h(px(24.0))
                            .rounded(px(crate::theme::radius::CONTROL))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .text_color(Colors::text_secondary())
                            .id("zoom-out-btn")
                            .hover(|style| style.bg(Colors::surface_hover()))
                            .on_click(move |_, window, cx| {
                                on_zoom_out_btn(&(), window, cx);
                            })
                            .child(
                                svg()
                                    .path(assets::ICON_MINUS_PATH)
                                    .w(px(12.0))
                                    .h(px(12.0))
                                    .text_color(Colors::text_secondary()),
                            ),
                    )
                    // Zoom readout label
                    .child(
                        div()
                            .px(px(4.0))
                            .text_size(px(9.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(Colors::text_muted())
                            .child({
                                let ppb =
                                    state.viewport.pixels_per_second * state.seconds_per_beat();
                                if ppb >= 100.0 {
                                    format!("{:.0} px/bt", ppb)
                                } else {
                                    format!("{:.1} px/bt", ppb)
                                }
                            }),
                    )
                    // Zoom In Button
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(24.0))
                            .h(px(24.0))
                            .rounded(px(crate::theme::radius::CONTROL))
                            .cursor(gpui::CursorStyle::PointingHand)
                            .text_color(Colors::text_secondary())
                            .id("zoom-in-btn")
                            .hover(|style| style.bg(Colors::surface_hover()))
                            .on_click(move |_, window, cx| {
                                on_zoom_in_btn(&(), window, cx);
                            })
                            .child(
                                svg()
                                    .path(assets::ICON_PLUS_PATH)
                                    .w(px(12.0))
                                    .h(px(12.0))
                                    .text_color(Colors::text_secondary()),
                            ),
                    ),
            )
    }
}

pub(crate) fn vertical_scrollbar(
    cx: &mut Context<Timeline>,
    scroll_y: f32,
    content_h: f32,
    view_h: f32,
    max_scroll: f32,
    content_top: f32,
) -> gpui::AnyElement {
    if max_scroll <= 0.5 || view_h <= 0.0 {
        return Empty.into_any_element();
    }
    let track_h = view_h;
    let thumb_h = ((view_h / content_h) * track_h).max(SCROLLBAR_MIN_THUMB);
    let progress = (scroll_y / max_scroll).clamp(0.0, 1.0);
    let thumb_top = progress * (track_h - thumb_h).max(0.0);

    let on_track_click = cx.listener(move |this, event: &gpui::MouseDownEvent, _w, cx| {
        // Position is in window space; convert to a fraction of the
        // scrollbar track. We approximate the track top as the click
        // y minus the thumb half-height when clicking above the thumb,
        // and snap the thumb center to the click otherwise.
        let click_y: f32 = event.position.y.into();
        // The scrollbar sits at top=RULER_HEIGHT inside the timeline.
        // Re-derive the local y by subtracting an estimated chrome
        // height; clamp with `max_scroll` so any over/under-estimate
        // still yields a valid scroll position.
        let local = (click_y - 36.0 - content_top).max(0.0);
        let frac = (local / track_h.max(1.0)).clamp(0.0, 1.0);
        this.state.set_scroll_immediate(
            this.state.viewport.scroll_x,
            (frac * max_scroll).clamp(0.0, max_scroll),
            f32::MAX,
            max_scroll,
        );
        cx.notify();
    });

    let on_thumb_drag = cx.listener(
        move |this, event: &gpui::DragMoveEvent<ScrollbarDrag>, _w, cx| {
            if event.drag(cx).axis != ScrollAxis::Vertical {
                return;
            }
            let y: f32 = event.event.position.y.into();
            let oy: f32 = event.bounds.origin.y.into();
            let track_range = (track_h - thumb_h).max(1.0);
            let local = (y - oy - thumb_h * 0.5).clamp(0.0, track_range);
            let frac = local / track_range;
            this.state.set_scroll_immediate(
                this.state.viewport.scroll_x,
                frac * max_scroll,
                f32::MAX,
                max_scroll,
            );
            cx.notify();
        },
    );

    div()
        .absolute()
        .top(px(content_top))
        .right(px(0.0))
        .bottom(px(0.0))
        .w(px(SCROLLBAR_THICKNESS))
        .id("timeline-vscroll")
        .on_mouse_down(gpui::MouseButton::Left, on_track_click)
        .on_drag(
            ScrollbarDrag {
                axis: ScrollAxis::Vertical,
            },
            |drag, _offset, _window, cx| cx.new(|_| drag.clone()),
        )
        .on_drag_move::<ScrollbarDrag>(on_thumb_drag)
        .child(
            div()
                .absolute()
                .top(px(thumb_top))
                .left(px(2.0))
                .right(px(2.0))
                .h(px(thumb_h))
                .rounded(px(crate::theme::radius::PILL))
                .bg(Colors::with_alpha(Colors::text_primary(), 0.2)),
        )
        .into_any_element()
}

pub(crate) fn horizontal_scrollbar(
    cx: &mut Context<Timeline>,
    scroll_x: f32,
    content_w: f32,
    view_w: f32,
    max_scroll: f32,
) -> gpui::AnyElement {
    if max_scroll <= 0.5 || view_w <= 0.0 {
        return Empty.into_any_element();
    }
    let track_w = view_w;
    let thumb_w = ((view_w / content_w) * track_w).max(SCROLLBAR_MIN_THUMB);
    let progress = (scroll_x / max_scroll).clamp(0.0, 1.0);
    let thumb_left = progress * (track_w - thumb_w).max(0.0);

    let on_track_click = cx.listener(move |this, event: &gpui::MouseDownEvent, _w, cx| {
        let click_x: f32 = event.position.x.into();
        let local = this.state.lane_x_from_window_x(click_x).max(0.0);
        let frac = (local / track_w.max(1.0)).clamp(0.0, 1.0);
        this.state.set_scroll_immediate(
            (frac * max_scroll).clamp(0.0, max_scroll),
            this.state.viewport.scroll_y,
            max_scroll,
            f32::MAX,
        );
        cx.notify();
    });

    let on_thumb_drag = cx.listener(
        move |this, event: &gpui::DragMoveEvent<ScrollbarDrag>, _w, cx| {
            if event.drag(cx).axis != ScrollAxis::Horizontal {
                return;
            }
            let x: f32 = event.event.position.x.into();
            let ox: f32 = event.bounds.origin.x.into();
            let track_range = (track_w - thumb_w).max(1.0);
            let local = (x - ox - thumb_w * 0.5).clamp(0.0, track_range);
            let frac = local / track_range;
            this.state.set_scroll_immediate(
                frac * max_scroll,
                this.state.viewport.scroll_y,
                max_scroll,
                f32::MAX,
            );
            cx.notify();
        },
    );

    div()
        .absolute()
        .bottom(px(0.0))
        .left(px(HEADER_WIDTH))
        .right(px(SCROLLBAR_THICKNESS))
        .h(px(SCROLLBAR_THICKNESS))
        .id("timeline-hscroll")
        .on_mouse_down(gpui::MouseButton::Left, on_track_click)
        .on_drag(
            ScrollbarDrag {
                axis: ScrollAxis::Horizontal,
            },
            |drag, _offset, _window, cx| cx.new(|_| drag.clone()),
        )
        .on_drag_move::<ScrollbarDrag>(on_thumb_drag)
        .child(
            div()
                .absolute()
                .left(px(thumb_left))
                .top(px(2.0))
                .bottom(px(2.0))
                .w(px(thumb_w))
                .rounded(px(crate::theme::radius::PILL))
                .bg(Colors::with_alpha(Colors::text_primary(), 0.2)),
        )
        .into_any_element()
}

/// Translucent arrangement range-selection rectangle. Pure render of
/// `state.arrangement_range` — UI-only, follows zoom/scroll, and never touches
/// the engine or marks the project dirty. Spans the affected tracks vertically
/// and the selected beat span horizontally. Non-interactive so it does not
/// intercept lane drags. Returns `None` when no range is active.
/// Per-pixel zoom rate for Ctrl + wheel on the arrangement.
const WHEEL_ZOOM_BASE: f32 = 1.0024;

/// Map a wheel delta onto a multiplicative zoom factor.
///
/// Wheel up is a *positive* delta on every platform, and the zoom helpers
/// multiply by this factor, so wheel up must yield a factor above 1 (zoom in).
/// The exponent is therefore deliberately **not** negated — doing so inverted
/// the gesture, making Ctrl + wheel up zoom out.
///
/// This is the opposite of the pan path, which does subtract the delta: panning
/// maps wheel motion onto the content origin (opposite by definition), whereas
/// zooming maps it onto a scale, where up simply means more.
pub(crate) fn wheel_zoom_factor(delta_y: f32) -> f32 {
    WHEEL_ZOOM_BASE.powf(delta_y)
}

/// Map a GPUI [`PinchEvent::delta`] onto a multiplicative zoom factor.
///
/// Platform backends already normalize the gesture: positive delta means zoom
/// in, and `0.1` means "+10%". Began/Ended phases emit `0.0` and are no-ops.
pub(crate) fn pinch_zoom_factor(delta: f32) -> f32 {
    (1.0 + delta).max(0.0001)
}

/// Resolve a pen-draw gesture's `(start_beat, end_beat)` into the final
/// `(clip_start, length_beats)` that will be committed. Both endpoints are
/// already resolved by the shared musical snap source (or Shift-bypassed), so
/// this helper only normalizes direction and enforces positive duration. Shared
/// by the live ghost preview and the commit so they can never disagree.
pub(crate) fn compute_pen_clip_span(
    _state: &TimelineState,
    start_beat: f32,
    end_beat: f32,
) -> (f32, f32) {
    use crate::components::timeline::timeline_state::{
        DEFAULT_MIDI_CLIP_BEATS, MIN_MIDI_CLIP_BEATS,
    };
    let (lo, hi) = normalize_range(start_beat, end_beat);
    let dragged_length = hi - lo;
    let length = if dragged_length <= f32::EPSILON {
        DEFAULT_MIDI_CLIP_BEATS
    } else {
        dragged_length.max(MIN_MIDI_CLIP_BEATS)
    };
    (lo, length)
}

/// Human-readable musical length, e.g. `1 bar`, `4 bars`, `2.5 bars`, `3.0 bt`.
pub(crate) fn format_clip_length(length_beats: f32, beats_per_bar: f32) -> String {
    let bpb = beats_per_bar.max(1.0);
    let bars = length_beats / bpb;
    if (bars - bars.round()).abs() < 1.0e-3 && bars >= 1.0 {
        let n = bars.round() as i32;
        format!("{} bar{}", n, if n == 1 { "" } else { "s" })
    } else if bars >= 1.0 {
        format!("{:.1} bars", bars)
    } else {
        format!("{:.1} bt", length_beats)
    }
}

pub(crate) fn format_arrangement_target_debug(target: &ArrangementHitTarget) -> String {
    match target {
        ArrangementHitTarget::EmptyArrangement {
            timeline_beat,
            track_id,
        } => format!("track_id={track_id:?}\ntimeline_beat={timeline_beat:.3}"),
        ArrangementHitTarget::TrackHeader { track_id } => format!("track_id={track_id}"),
        ArrangementHitTarget::TrackLane {
            track_id,
            timeline_beat,
        } => format!("track_id={track_id}\ntimeline_beat={timeline_beat:.3}"),
        ArrangementHitTarget::AudioClip {
            track_id,
            clip_id,
            timeline_beat,
            local_beat,
        }
        | ArrangementHitTarget::MidiClip {
            track_id,
            clip_id,
            timeline_beat,
            local_beat,
        }
        | ArrangementHitTarget::VideoClip {
            track_id,
            clip_id,
            timeline_beat,
            local_beat,
        } => format!(
            "track_id={track_id}\nclip_id={clip_id}\ntimeline_beat={timeline_beat:.3}\nlocal_beat={local_beat:.3}"
        ),
        ArrangementHitTarget::Ruler { timeline_beat } => {
            format!("timeline_beat={timeline_beat:.3}")
        }
        ArrangementHitTarget::Marker {
            marker_id,
            timeline_beat,
        } => format!("marker_id={marker_id}\ntimeline_beat={timeline_beat:.3}"),
        ArrangementHitTarget::AutomationLane {
            track_id,
            lane_id,
            timeline_beat,
        } => format!("track_id={track_id}\nlane_id={lane_id}\ntimeline_beat={timeline_beat:.3}"),
    }
}

fn file_drop_hint_label(paths: &[std::path::PathBuf]) -> &'static str {
    if paths.iter().any(|path| is_supported_audio_ext(path)) {
        "Drop Audio to import"
    } else if paths.iter().any(|path| is_supported_midi_ext(path)) {
        "Drop MIDI to import"
    } else if paths
        .iter()
        .any(|path| sphere_video_player::is_supported_video_path(path))
    {
        "Drop Video to import"
    } else {
        "Drop files to import"
    }
}

fn file_drop_hint_overlay(hint: &FileDropHint, state: &TimelineState) -> Option<gpui::AnyElement> {
    let window_x: f32 = hint.position.x.into();
    let window_y: f32 = hint.position.y.into();
    let lane_x = state.lane_x_from_window_x(window_x).max(0.0);
    let lane_y = (window_y - APP_CHROME_HEIGHT - state.arrangement_content_top()).max(0.0);
    let beat = state.snap_beats(state.x_to_beats(lane_x)).max(0.0);
    let track_index = state.track_index_at_y(lane_y);
    let row_layout = state.track_row_layout();
    let (target_top, target_h, target_name, target_color) = if let Some(index) = track_index {
        let row = row_layout.row_for_index(index)?;
        let track = state.tracks.get(index)?;
        (
            row.y - state.viewport.scroll_y,
            row.height,
            track.name.clone(),
            track.color,
        )
    } else {
        let y = state.total_track_rows_height() - state.viewport.scroll_y;
        (
            y.max(0.0),
            DEFAULT_TRACK_HEIGHT,
            "New MIDI/Audio Track".to_string(),
            Colors::accent_primary(),
        )
    };

    let marker_x = state.beats_to_x(beat).max(0.0);
    let label_top = (target_top + 8.0).max(4.0);
    let label_left = (marker_x + 10.0).max(8.0);
    let accent = Colors::accent_primary();

    let lane_highlight = div()
        .absolute()
        .left_0()
        .right_0()
        .top(px(target_top.max(0.0)))
        .h(px(target_h.max(24.0)))
        .bg(Colors::with_alpha(target_color, 0.10))
        .border_t(px(1.0))
        .border_b(px(1.0))
        .border_color(Colors::with_alpha(accent, 0.42))
        .with_animation(
            "timeline-file-drop-lane-pulse",
            Animation::new(Duration::from_millis(900))
                .repeat()
                .with_easing(pulsating_between(0.18, 0.38)),
            move |this, delta| this.bg(Colors::with_alpha(target_color, delta)),
        );

    let beat_guide = div()
        .absolute()
        .left(px((marker_x - 0.5).max(0.0)))
        .top_0()
        .bottom_0()
        .w(px(1.0))
        .bg(Colors::with_alpha(accent, 0.65));

    let label = div()
        .absolute()
        .left(px(label_left))
        .top(px(label_top))
        .px(px(8.0))
        .py(px(5.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::with_alpha(accent, 0.62))
        .bg(Colors::with_alpha(Colors::surface_panel(), 0.96))
        .shadow_lg()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(hint.label),
        )
        .child(
            div()
                .text_size(px(9.0))
                .text_color(Colors::text_muted())
                .child(format!("{} · {}", target_name, state.format_position(beat))),
        )
        .with_animation(
            "timeline-file-drop-label-pulse",
            Animation::new(Duration::from_millis(900))
                .repeat()
                .with_easing(pulsating_between(0.78, 1.0)),
            |this, delta| this.opacity(delta),
        );

    Some(
        div()
            .absolute()
            .inset_0()
            .child(lane_highlight)
            .child(beat_guide)
            .child(label)
            .into_any_element(),
    )
}

/// Animated arrangement ghost for an Alt-drag clone. The target bounds are
/// derived from the same snap/track geometry used by the drop command, so the
/// visual preview and committed duplicate cannot disagree.
fn clip_clone_hint_overlay(
    hint: &ClipCloneHint,
    state: &TimelineState,
) -> Option<gpui::AnyElement> {
    let (_, source_clip) = state.find_clip(&hint.clip_id)?;
    let target_track = state.tracks.get(hint.target_track_index)?;
    let row_layout = state.track_row_layout();
    let row = row_layout.row_for_index(hint.target_track_index)?;

    let x = state.beats_to_x(hint.start_beat).max(0.0);
    let width = (state.beats_to_x(hint.start_beat + source_clip.duration_beats) - x).max(10.0);
    let pad = 7.0;
    let y = row.y - state.viewport.scroll_y + pad;
    let height = (row.height - pad * 2.0).max(8.0);
    let color = target_track.color;
    let kind = match &source_clip.clip_type {
        ClipType::Audio { .. } => "Audio",
        ClipType::Midi { .. } => "MIDI",
        ClipType::Video { .. } => "Video",
    };
    let label = format!(
        "Copy {kind} to {} · {}",
        target_track.name,
        state.format_position(hint.start_beat)
    );

    let ghost = div()
        .absolute()
        .left(px(x))
        .top(px(y.max(0.0)))
        .w(px(width))
        .h(px(height))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::with_alpha(color, 0.6))
        .bg(Colors::with_alpha(color, 0.12))
        .child(
            div()
                .h_full()
                .px(px(7.0))
                .flex()
                .items_center()
                .truncate()
                .text_size(px(9.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(label),
        )
        .with_animation(
            "timeline-clip-clone-ghost-pulse",
            Animation::new(Duration::from_millis(760))
                .repeat()
                .with_easing(pulsating_between(0.30, 0.76)),
            move |this, delta| {
                this.bg(Colors::with_alpha(color, 0.06 + delta * 0.16))
                    .border_color(Colors::with_alpha(color, delta))
            },
        );

    Some(div().absolute().inset_0().child(ghost).into_any_element())
}

/// Live ghost-clip overlay for the in-flight pen MIDI clip draw. Translucent,
/// track-colored, with a pulsing outline and a floating length/range label so
/// the user sees the exact bounds and musical length before releasing.
fn pen_clip_draw_overlay(
    preview: &ClipDrawPreview,
    state: &TimelineState,
) -> Option<gpui::AnyElement> {
    let track_index = state.tracks.iter().position(|t| t.id == preview.track_id)?;
    let track_color = state.tracks[track_index].color;

    let (clip_start, length) =
        compute_pen_clip_span(state, preview.start_beat, preview.current_beat);
    let clip_end = clip_start + length;

    let x_lo = state.beats_to_x(clip_start);
    let width = (state.beats_to_x(clip_end) - x_lo).max(2.0);
    let row_layout = state.track_row_layout();
    let row = row_layout.row_for_index(track_index)?;
    let pad = 7.0;
    let top = row.y - state.viewport.scroll_y + pad;
    let height = (row.height - pad * 2.0).max(1.0);

    let bpb = state.beats_per_bar();
    let length_label = format_clip_length(length, bpb);
    let range_label = format!(
        "{} → {}",
        state.format_position(clip_start),
        state.format_position(clip_end)
    );

    let ghost_fill = Colors::with_alpha(track_color, 0.16);
    let label_text = Colors::with_alpha(Colors::text_primary(), 0.92);

    // Ghost clip body — translucent track-colored fill with a pulsing outline so
    // it reads as "in creation". The pulse animates on its own frames, so it
    // stays alive even when the cursor is held still.
    let body = div()
        .absolute()
        .left(px(x_lo))
        .top(px(top))
        .w(px(width))
        .h(px(height))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(ghost_fill)
        .border(px(1.0))
        .border_color(Colors::with_alpha(track_color, 0.85))
        .overflow_hidden()
        .flex()
        .flex_col()
        .justify_between()
        // Title placeholder.
        .child(
            div()
                .px(px(6.0))
                .pt(px(4.0))
                .text_size(px(9.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(label_text)
                .truncate()
                .child("New MIDI Clip"),
        )
        // Bottom length readout, mirroring the committed clip's label bar.
        .child(
            div()
                .h(px(14.0))
                .w_full()
                .bg(Colors::with_alpha(Colors::surface_panel_alt(), 0.85))
                .border_t(px(1.0))
                .border_color(Colors::divider())
                .px(px(6.0))
                .flex()
                .items_center()
                .justify_end()
                .text_size(px(8.0))
                .text_color(Colors::text_secondary())
                .child(format!("{:.1} bt", length)),
        )
        .with_animation(
            "pen-clip-draw-pulse",
            Animation::new(Duration::from_millis(1100))
                .repeat()
                .with_easing(pulsating_between(0.35, 0.85)),
            move |this, delta| this.border_color(Colors::with_alpha(track_color, delta)),
        );

    // Floating musical-length label, pinned just above the ghost clip (or below
    // it when the clip sits at the very top of the lane area).
    let label_below = top < 26.0;
    let label = div()
        .absolute()
        .left(px(x_lo + 2.0))
        .map(|el| {
            if label_below {
                el.top(px(top + height + 4.0))
            } else {
                el.top(px((top - 22.0).max(0.0)))
            }
        })
        .px(px(6.0))
        .py(px(2.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .bg(Colors::with_alpha(Colors::surface_panel(), 0.96))
        .border(px(1.0))
        .border_color(Colors::with_alpha(track_color, 0.6))
        .shadow_lg()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .text_size(px(9.0))
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(label_text)
                .child(length_label),
        )
        .child(div().text_color(Colors::text_muted()).child(range_label));

    // Subtle full-height guide at the clip end so the snapped end position reads
    // clearly against the grid.
    let end_x = state.beats_to_x(clip_end);
    let end_guide = div()
        .absolute()
        .left(px((end_x - 0.5).max(0.0)))
        .top_0()
        .bottom_0()
        .w(px(1.0))
        .bg(Colors::with_alpha(track_color, 0.45));

    Some(
        div()
            .absolute()
            .inset_0()
            .child(end_guide)
            .child(body)
            .child(label)
            .into_any_element(),
    )
}

pub(crate) fn arrangement_range_overlay(state: &TimelineState) -> Option<gpui::AnyElement> {
    let range = state.arrangement_range.as_ref()?;
    let (start_beat, end_beat) = range.as_f32_range();
    let (lo, hi) = normalize_range(start_beat, end_beat);
    let x_lo = state.beats_to_x(lo);
    let width = (state.beats_to_x(hi) - x_lo).max(1.0);

    // Vertical span follows the affected track ids; an empty set covers the
    // whole lane area (e.g. a horizontal-only time range).
    let (y_top, height) = {
        let mut min_idx = usize::MAX;
        let mut max_idx = 0usize;
        for (idx, track) in state.tracks.iter().enumerate() {
            if range.track_ids.iter().any(|id| id == &track.id) {
                min_idx = min_idx.min(idx);
                max_idx = max_idx.max(idx);
            }
        }
        if min_idx == usize::MAX {
            (0.0_f32, state.viewport.track_area_height.max(0.0))
        } else {
            let row_layout = state.track_row_layout();
            let top = row_layout
                .row_for_index(min_idx)
                .map(|row| row.y)
                .unwrap_or(0.0);
            let bottom = row_layout
                .row_for_index(max_idx)
                .map(|row| row.y + row.height)
                .unwrap_or(top);
            let y_top = top - state.viewport.scroll_y;
            let h = (bottom - top).max(0.0);
            (y_top, h)
        }
    };

    Some(
        div()
            .absolute()
            .left(px(x_lo))
            .top(px(y_top))
            .w(px(width))
            .h(px(height))
            .bg(Colors::with_alpha(Colors::accent_primary(), 0.14))
            .border(px(1.0))
            .border_color(Colors::with_alpha(Colors::accent_primary(), 0.7))
            .into_any_element(),
    )
}

pub(crate) fn timeline_marker_region_overlay(state: &TimelineState) -> Option<gpui::AnyElement> {
    if state.markers.is_empty() && state.regions.is_empty() {
        return None;
    }

    let (visible_start, visible_end) = state.visible_beat_range(state.viewport.viewport_width);
    let body_height = state
        .viewport
        .track_area_height
        .max(state.viewport.viewport_height);
    let mut children: Vec<gpui::AnyElement> = Vec::new();

    for region in &state.regions {
        let (start, end) = region.normalized_range();
        if end < visible_start as f64 || start > visible_end as f64 {
            continue;
        }
        let x = state.beats_to_x(start as f32);
        let width = (state.beats_to_x(end as f32) - x).max(1.0);
        let color = crate::color::parse_hex_color(&region.color_hex)
            .unwrap_or_else(|_| Colors::accent_success());
        children.push(
            div()
                .absolute()
                .left(px(x))
                .top_0()
                .h(px(body_height))
                .w(px(width))
                .bg(Colors::with_alpha(color, 0.08))
                .border_l(px(1.0))
                .border_r(px(1.0))
                .border_color(Colors::with_alpha(color, 0.35))
                .into_any_element(),
        );
    }

    for marker in &state.markers {
        if marker.beat < visible_start as f64 || marker.beat > visible_end as f64 {
            continue;
        }
        let x = state.beats_to_x(marker.beat as f32);
        let color = crate::color::parse_hex_color(&marker.color_hex)
            .unwrap_or_else(|_| Colors::accent_primary());
        children.push(
            div()
                .absolute()
                .left(px(x))
                .top_0()
                .h(px(body_height))
                .w(px(1.0))
                .bg(Colors::with_alpha(color, 0.48))
                .into_any_element(),
        );
    }

    if children.is_empty() {
        return None;
    }

    Some(
        div()
            .absolute()
            .inset_0()
            .children(children)
            .into_any_element(),
    )
}

#[cfg(test)]
mod midi_clip_draw_tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        DEFAULT_MIDI_CLIP_BEATS, MIN_MIDI_CLIP_BEATS,
    };

    /// Ctrl + wheel up must zoom *in*. Regression guard: the exponent was
    /// negated, so wheel up produced a factor below 1 and zoomed out.
    #[test]
    fn ctrl_wheel_up_zooms_in_and_down_zooms_out() {
        assert!(
            wheel_zoom_factor(30.0) > 1.0,
            "wheel up must zoom in, got {}",
            wheel_zoom_factor(30.0)
        );
        assert!(
            wheel_zoom_factor(-30.0) < 1.0,
            "wheel down must zoom out, got {}",
            wheel_zoom_factor(-30.0)
        );
        assert_eq!(wheel_zoom_factor(0.0), 1.0, "no delta must not zoom");
    }

    /// Trackpad pinch delta is already a fractional scale change (`0.1` = +10%).
    #[test]
    fn pinch_in_zooms_in_and_out_zooms_out() {
        assert!(
            pinch_zoom_factor(0.1) > 1.0,
            "pinch out (positive) must zoom in, got {}",
            pinch_zoom_factor(0.1)
        );
        assert!(
            pinch_zoom_factor(-0.1) < 1.0,
            "pinch in (negative) must zoom out, got {}",
            pinch_zoom_factor(-0.1)
        );
        assert_eq!(pinch_zoom_factor(0.0), 1.0, "zero pinch must not zoom");
    }

    /// The factor has to actually move `pixels_per_second` the right way, not
    /// merely sit on the right side of 1.0.
    #[test]
    fn ctrl_wheel_up_increases_pixels_per_second() {
        let mut state = TimelineState::default();
        let before = state.viewport.pixels_per_second;
        state.zoom_by(wheel_zoom_factor(30.0), 0.0);
        assert!(
            state.viewport.pixels_per_second > before,
            "wheel up should widen the timeline: {before} -> {}",
            state.viewport.pixels_per_second
        );

        let mut state = TimelineState::default();
        let before = state.viewport.pixels_per_second;
        state.zoom_by(wheel_zoom_factor(-30.0), 0.0);
        assert!(
            state.viewport.pixels_per_second < before,
            "wheel down should narrow the timeline: {before} -> {}",
            state.viewport.pixels_per_second
        );
    }

    #[test]
    fn drag_span_supports_both_directions() {
        let state = TimelineState::default();
        assert_eq!(compute_pen_clip_span(&state, 2.0, 5.0), (2.0, 3.0));
        assert_eq!(compute_pen_clip_span(&state, 5.0, 2.0), (2.0, 3.0));
    }

    #[test]
    fn click_uses_default_and_short_drag_stays_positive() {
        let state = TimelineState::default();
        assert_eq!(
            compute_pen_clip_span(&state, 2.0, 2.0),
            (2.0, DEFAULT_MIDI_CLIP_BEATS)
        );
        assert_eq!(
            compute_pen_clip_span(&state, 2.0, 2.01),
            (2.0, MIN_MIDI_CLIP_BEATS)
        );
    }
}

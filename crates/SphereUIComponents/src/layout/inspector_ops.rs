//! Inspector edit callbacks. Every mutation lands in the same `TimelineState`
//! the TrackHeader and Mixer read, so an Inspector edit is reflected
//! everywhere. Mirrors the structure of [`build_mixer_callbacks`].
//!
//! Dirty policy (see plan):
//! * name / color / mute / solo / arm / input-monitor → project dirty only.
//! * volume / pan → project dirty + one realtime `update_track_param`.
//! Selection-only changes never mark anything dirty.

use std::sync::Arc;

use gpui::{App, Entity, Window};

use crate::components::edit::EditCommand;
use crate::components::inspector_debug;
use crate::components::panel::{InspectorCallbacks, InspectorRoutingCombo};
use crate::components::plugin_picker::PluginInsertKind;
use crate::components::reorder::{InsertDrop, InsertDropCb};
use crate::components::timeline::timeline_state::{
    vsti_output_bus_strip_indices, vsti_output_child_channels_for_bus_layout,
    AudioClipStretchState, TrackAudioFormat, TrackEditScope, TrackMidiInputRouting,
    TrackOutputRouting,
};
use crate::overlay::OverlayAnchor;
use sphere_midi_service::mpe::MpeTrackConfiguration;

use super::engine_snapshot::{apply_engine_track_input_state, volume_norm_to_linear};
use super::StudioLayout;

/// Stereo channel pair (1-based) of the VSTi output bus that owns `channel`,
/// from the real per-bus channel counts. A mono bus reports `(ch, ch)` so it is
/// routed as a duplicated stereo pair. Falls back to `(channel, channel)` when
/// the layout is unknown so a single tick still routes both sides.
fn vsti_output_bus_pair_for_channel(bus_counts: &[u8], channel: u8) -> (u8, u8) {
    for bus_index in vsti_output_bus_strip_indices(bus_counts) {
        if let Some((l, r)) = vsti_output_child_channels_for_bus_layout(bus_counts, bus_index) {
            if channel == l || channel == r {
                return (l, r);
            }
        }
    }
    (channel, channel)
}

type StrCb = Arc<dyn Fn(&String, &mut Window, &mut App) + 'static>;
type StrF32Cb = Arc<dyn Fn(&(String, f32), &mut Window, &mut App) + 'static>;
type InputRoutingCb = Arc<
    dyn Fn(&(String, crate::input_routing::TrackInputSelection), &mut Window, &mut App) + 'static,
>;
type OutputRoutingCb = Arc<dyn Fn(&(String, TrackOutputRouting), &mut Window, &mut App) + 'static>;
type AudioFormatCb = Arc<dyn Fn(&(String, TrackAudioFormat), &mut Window, &mut App) + 'static>;
type MidiInputCb = Arc<dyn Fn(&(String, TrackMidiInputRouting), &mut Window, &mut App) + 'static>;
type MidiChannelCb = Arc<dyn Fn(&(String, Option<u8>), &mut Window, &mut App) + 'static>;
type MpeConfigurationCb =
    Arc<dyn Fn(&(String, MpeTrackConfiguration), &mut Window, &mut App) + 'static>;
type InsertPairCb = Arc<dyn Fn(&(String, String), &mut Window, &mut App) + 'static>;
type InsertOpenCb = Arc<dyn Fn(&(String, usize, String), &mut Window, &mut App) + 'static>;
type InsertPickerCb = Arc<dyn Fn(&(String, usize, bool), &mut Window, &mut App) + 'static>;
type InsertOutputChannelCb =
    Arc<dyn Fn(&(String, String, u8, bool), &mut Window, &mut App) + 'static>;
type ClipF32Cb = Arc<dyn Fn(&(String, f32), &mut Window, &mut App) + 'static>;
type ClipBoolCb = Arc<dyn Fn(&(String, bool), &mut Window, &mut App) + 'static>;
type ClipStretchCb = Arc<dyn Fn(&(String, AudioClipStretchState), &mut Window, &mut App) + 'static>;

fn clip_dsp_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_CLIP_DSP_DEBUG").is_some())
}

fn stretch_commit_field(
    prev: &AudioClipStretchState,
    next: &AudioClipStretchState,
) -> &'static str {
    if (prev.stretch_ratio - next.stretch_ratio).abs() > f64::EPSILON {
        "ratio"
    } else if prev.mode != next.mode {
        "mode"
    } else if prev.algorithm != next.algorithm {
        "algorithm"
    } else if prev.preserve_pitch != next.preserve_pitch {
        "preserve_pitch"
    } else if (prev.pitch_shift_semitones - next.pitch_shift_semitones).abs() > f32::EPSILON {
        "pitch"
    } else if prev.reverse != next.reverse {
        "reverse"
    } else if prev.warp_markers.len() != next.warp_markers.len() {
        "warp_markers"
    } else {
        "other"
    }
}

impl StudioLayout {
    /// Callbacks for the Inspector's shared colour picker.
    ///
    /// Every gesture — preset swatch, recent swatch, hue strip, saturation /
    /// value area — commits through `apply_inspector_color`, so a drag is a live
    /// edit of the real track colour rather than a preview that only lands on
    /// close. `track_id` is the current selection; opening the picker binds it,
    /// and the binding (not the live selection) is what later edits target.
    pub(crate) fn build_inspector_color_picker_callbacks(
        &self,
        owner: Entity<Self>,
        track_id: Option<String>,
    ) -> crate::components::ColorPickerCallbacks {
        /// Mutate the host picker, then push the resulting draft colour to the
        /// bound track. Shared by every drag/click gesture below.
        fn commit(
            owner: &Entity<StudioLayout>,
            cx: &mut App,
            edit: impl FnOnce(&mut crate::components::ColorPickerState),
        ) {
            let _ = owner.update(cx, |this, cx| {
                edit(&mut this.inspector_name_edit.color_picker);
                let color = this.inspector_name_edit.color_picker.draft;
                this.apply_inspector_color(color, cx);
                cx.notify();
            });
        }

        crate::components::ColorPickerCallbacks {
            on_toggle: Arc::new({
                let owner = owner.clone();
                move |window: &mut Window, cx: &mut App| {
                    let track_id = track_id.clone();
                    let _ = owner.update(cx, |this, cx| {
                        if this.inspector_name_edit.color_picker.open {
                            this.close_inspector_color_picker(window, cx);
                        } else if let Some(track_id) = track_id {
                            this.open_inspector_color_picker(&track_id, cx);
                        }
                    });
                }
            }),
            on_close: Arc::new({
                let owner = owner.clone();
                move |window: &mut Window, cx: &mut App| {
                    let _ = owner.update(cx, |this, cx| {
                        this.close_inspector_color_picker(window, cx);
                    });
                }
            }),
            on_pick: Arc::new({
                let owner = owner.clone();
                move |color: gpui::Rgba, _w: &mut Window, cx: &mut App| {
                    commit(&owner, cx, |picker| picker.set_color(color));
                }
            }),
            on_hue: Arc::new({
                let owner = owner.clone();
                move |hue: f32, _w: &mut Window, cx: &mut App| {
                    commit(&owner, cx, |picker| picker.set_hue(hue));
                }
            }),
            on_sv: Arc::new({
                let owner = owner.clone();
                move |saturation: f32, value: f32, _w: &mut Window, cx: &mut App| {
                    commit(&owner, cx, |picker| {
                        picker.set_saturation_value(saturation, value)
                    });
                }
            }),
            // Auto Colour is not offered by the Inspector picker (an existing
            // track always has a concrete stored colour), so this is unreachable
            // from the UI; keep it inert rather than inventing a meaning.
            on_auto: Arc::new(|_on: bool, _w: &mut Window, _cx: &mut App| {}),
        }
    }

    pub(crate) fn build_inspector_callbacks(&self, owner: Entity<Self>) -> InspectorCallbacks {
        let audio_engine = self.audio_bridge.engine.clone();
        let timeline_vol = self.timeline.clone();
        let owner_vol = owner.clone();
        let on_volume: StrF32Cb = Arc::new(move |(id, v): &(String, f32), _w, cx| {
            let id = id.clone();
            let v = *v;
            let changed = timeline_vol.update(cx, |timeline, cx| {
                let Some(prev) = timeline.state.find_track(&id).map(|track| track.volume) else {
                    return false;
                };
                if (prev - v).abs() <= 1.0e-5 {
                    return false;
                }
                timeline.run_edit_command(
                    EditCommand::SetTrackVolume {
                        track_id: id.clone(),
                        prev,
                        next: v,
                    },
                    cx,
                );
                true
            });
            if changed {
                StudioLayout::defer_update(&owner_vol, cx, |this, cx| {
                    this.push_mixer_snapshot_to_window(cx);
                });
                if let Some(engine) = audio_engine.as_ref() {
                    let _ =
                        engine.update_track_param(&id, "volume", volume_norm_to_linear(v) as f64);
                }
            }
        });
        let timeline_volume_start = self.timeline.clone();
        let on_volume_drag_start: StrF32Cb = Arc::new(move |(id, v): &(String, f32), _w, cx| {
            timeline_volume_start.update(cx, |timeline, cx| {
                timeline.state.begin_track_volume_preview(id, *v);
                cx.notify();
            });
        });
        let timeline_volume_preview = self.timeline.clone();
        let audio_engine_volume_preview = self.audio_bridge.engine.clone();
        let on_volume_drag_preview: StrF32Cb = Arc::new(move |(id, v): &(String, f32), _w, cx| {
            let changed = timeline_volume_preview.update(cx, |timeline, cx| {
                let changed = timeline.state.set_track_volume_preview(id, *v);
                if changed {
                    cx.notify();
                }
                changed
            });
            if changed {
                if let Some(engine) = audio_engine_volume_preview.as_ref() {
                    let _ =
                        engine.update_track_param(id, "volume", volume_norm_to_linear(*v) as f64);
                }
            }
        });
        let timeline_volume_commit = self.timeline.clone();
        let owner_volume_commit = owner.clone();
        let audio_engine_volume_commit = self.audio_bridge.engine.clone();
        let on_volume_drag_commit: StrCb = Arc::new(move |id: &String, _w, cx| {
            let id = id.clone();
            let committed = timeline_volume_commit.update(cx, |timeline, cx| {
                let committed = timeline.state.commit_track_volume_preview(&id);
                if let Some((prev, next)) = committed {
                    if (prev - next).abs() > 1.0e-5 {
                        timeline.record_executed_command(
                            EditCommand::SetTrackVolume {
                                track_id: id.clone(),
                                prev,
                                next,
                            },
                            cx,
                        );
                    }
                }
                committed
            });
            if let Some((_prev, next)) = committed {
                if let Some(engine) = audio_engine_volume_commit.as_ref() {
                    let _ = engine.update_track_param(
                        &id,
                        "volume",
                        volume_norm_to_linear(next) as f64,
                    );
                }
                StudioLayout::defer_update(&owner_volume_commit, cx, |this, cx| {
                    this.push_mixer_snapshot_to_window(cx);
                });
            }
        });

        let audio_engine = self.audio_bridge.engine.clone();
        let timeline_pan = self.timeline.clone();
        let owner_pan = owner.clone();
        let on_pan: StrF32Cb = Arc::new(move |(id, v): &(String, f32), _w, cx| {
            let id = id.clone();
            let v = *v;
            let changed = timeline_pan.update(cx, |timeline, cx| {
                let Some(prev) = timeline.state.find_track(&id).map(|track| track.pan) else {
                    return false;
                };
                if (prev - v).abs() <= 1.0e-5 {
                    return false;
                }
                timeline.run_edit_command(
                    EditCommand::SetTrackPan {
                        track_id: id.clone(),
                        prev,
                        next: v,
                    },
                    cx,
                );
                true
            });
            if changed {
                StudioLayout::defer_update(&owner_pan, cx, |this, cx| {
                    this.push_mixer_snapshot_to_window(cx);
                });
                if let Some(engine) = audio_engine.as_ref() {
                    let _ = engine.update_track_param(&id, "pan", v as f64);
                }
            }
        });
        let timeline_pan_start = self.timeline.clone();
        let on_pan_drag_start: StrCb = Arc::new(move |id: &String, _w, cx| {
            timeline_pan_start.update(cx, |timeline, cx| {
                timeline.state.begin_track_pan_preview(id);
                cx.notify();
            });
        });
        let timeline_pan_preview = self.timeline.clone();
        let audio_engine_pan_preview = self.audio_bridge.engine.clone();
        let on_pan_drag_preview: StrF32Cb = Arc::new(move |(id, v): &(String, f32), _w, cx| {
            let changed = timeline_pan_preview.update(cx, |timeline, cx| {
                let changed = timeline.state.set_track_pan_preview(id, *v);
                if changed {
                    cx.notify();
                }
                changed
            });
            if changed {
                if let Some(engine) = audio_engine_pan_preview.as_ref() {
                    let _ = engine.update_track_param(id, "pan", *v as f64);
                }
            }
        });
        let timeline_pan_commit = self.timeline.clone();
        let owner_pan_commit = owner.clone();
        let audio_engine_pan_commit = self.audio_bridge.engine.clone();
        let on_pan_drag_commit: StrCb = Arc::new(move |id: &String, _w, cx| {
            let id = id.clone();
            let committed = timeline_pan_commit.update(cx, |timeline, cx| {
                let committed = timeline.state.commit_track_pan_preview(&id);
                if let Some((prev, next)) = committed {
                    if (prev - next).abs() > 1.0e-5 {
                        timeline.record_executed_command(
                            EditCommand::SetTrackPan {
                                track_id: id.clone(),
                                prev,
                                next,
                            },
                            cx,
                        );
                    }
                }
                committed
            });
            if let Some((_prev, next)) = committed {
                if let Some(engine) = audio_engine_pan_commit.as_ref() {
                    let _ = engine.update_track_param(&id, "pan", next as f64);
                }
                StudioLayout::defer_update(&owner_pan_commit, cx, |this, cx| {
                    this.push_mixer_snapshot_to_window(cx);
                });
            }
        });

        let timeline_auto = self.timeline.clone();
        let owner_auto = owner.clone();
        let on_toggle_volume_automation_read: StrCb = Arc::new(move |id: &String, _w, cx| {
            let id = id.clone();
            let changed = timeline_auto.update(cx, |timeline, cx| {
                let prev = timeline
                    .state
                    .find_track(&id)
                    .map(|track| track.volume_automation_read);
                let Some(prev) = prev else {
                    return false;
                };
                timeline.run_edit_command(
                    EditCommand::SetTrackVolumeAutomationRead {
                        track_id: id.clone(),
                        prev,
                        next: !prev,
                    },
                    cx,
                );
                true
            });
            if changed {
                inspector_debug(&format!("toggle volume automation read track={id}"));
                // Persisted and undoable. Refresh the runtime once on commit so
                // playback immediately follows the restored read/bypass state.
                StudioLayout::defer_update(&owner_auto, cx, |this, cx| {
                    this.audio_bridge.project_dirty = true;
                    this.schedule_audio_project_sync(cx, false, "inspector_volume_automation_read");
                    this.push_mixer_snapshot_to_window(cx);
                });
            }
        });

        let on_toggle_mute = self.track_toggle_cb(owner.clone(), TrackToggle::Mute);
        let on_toggle_solo = self.track_toggle_cb(owner.clone(), TrackToggle::Solo);
        let on_toggle_arm = self.track_toggle_cb(owner.clone(), TrackToggle::Arm);
        let on_toggle_input = self.track_toggle_cb(owner.clone(), TrackToggle::Input);
        let on_set_input_routing = self.input_routing_cb(owner.clone());
        let on_set_output_routing = self.output_routing_cb(owner.clone());
        let on_set_audio_format = self.audio_format_cb(owner.clone());
        let on_set_midi_input = self.midi_input_cb(owner.clone());
        let on_set_midi_channel = self.midi_channel_cb(owner.clone());
        let on_set_mpe_configuration = self.mpe_configuration_cb(owner.clone());
        let on_open_insert_picker = self.insert_picker_cb(owner.clone());
        let on_remove_insert = self.remove_insert_cb(owner.clone());
        let on_toggle_insert_bypass = self.toggle_insert_bypass_cb(owner.clone());
        let on_toggle_insert_enabled = self.toggle_insert_enabled_cb(owner.clone());
        let on_toggle_insert_output_channel = self.toggle_insert_output_channel_cb(owner.clone());
        let on_drop_insert = self.drop_insert_cb(owner.clone());
        let on_open_insert_editor = self.open_insert_editor_cb(owner.clone());
        let on_set_clip_start = self.set_clip_start_cb(owner.clone());
        let on_set_clip_length = self.set_clip_length_cb(owner.clone());
        let on_set_clip_gain = self.set_clip_gain_cb(owner.clone());
        let on_set_clip_muted = self.set_clip_muted_cb(owner.clone());
        let on_set_clip_stretch = self.set_clip_stretch_cb(owner.clone());
        let timeline_clip_begin = self.timeline.clone();
        let on_begin_clip_property_drag: StrCb = Arc::new(move |clip_id: &String, _w, cx| {
            timeline_clip_begin.update(cx, |timeline, _cx| {
                timeline.begin_inspector_clip_gesture(clip_id);
            });
        });
        let timeline_clip_commit = self.timeline.clone();
        let owner_clip_commit = owner.clone();
        let on_commit_clip_property_drag: StrCb = Arc::new(move |clip_id: &String, _w, cx| {
            let clip_id = clip_id.clone();
            let changed = timeline_clip_commit.update(cx, |timeline, cx| {
                timeline.commit_inspector_clip_gesture(&clip_id, cx)
            });
            if changed {
                StudioLayout::defer_update(&owner_clip_commit, cx, |this, cx| {
                    this.mark_engine_media_dirty();
                    this.schedule_audio_project_sync(cx, false, "inspector_clip_drag_commit");
                    cx.notify();
                });
            }
        });
        let timeline_clip_start_preview = self.timeline.clone();
        let on_preview_clip_start: ClipF32Cb =
            Arc::new(move |(clip_id, value): &(String, f32), _w, cx| {
                timeline_clip_start_preview.update(cx, |timeline, cx| {
                    if timeline.state.set_clip_start(clip_id, *value) {
                        cx.notify();
                    }
                });
            });
        let timeline_clip_length_preview = self.timeline.clone();
        let on_preview_clip_length: ClipF32Cb =
            Arc::new(move |(clip_id, value): &(String, f32), _w, cx| {
                timeline_clip_length_preview.update(cx, |timeline, cx| {
                    if timeline.state.set_clip_length_trimming(clip_id, *value) {
                        cx.notify();
                    }
                });
            });
        let timeline_clip_gain_preview = self.timeline.clone();
        let on_preview_clip_gain: ClipF32Cb =
            Arc::new(move |(clip_id, value): &(String, f32), _w, cx| {
                timeline_clip_gain_preview.update(cx, |timeline, cx| {
                    if timeline.state.set_clip_gain(clip_id, *value) {
                        cx.notify();
                    }
                });
            });
        let on_preview_clip_stretch = self.preview_clip_stretch_cb();
        let on_clip_stretch_auto_find_bpm = self.clip_stretch_auto_find_cb(owner.clone());
        let on_clip_warp_clear = self.clip_warp_clear_cb(owner.clone());
        let on_open_clip_bottom_editor = self.open_clip_bottom_editor_cb(owner.clone());
        let on_open_clip_external_midi_editor =
            self.open_clip_external_midi_editor_cb(owner.clone());
        let on_open_soundfont_player = self.open_soundfont_player_cb(owner.clone());

        let open_routing_combo = self.overlay.inspector_routing_combo;
        let owner_routing_combo = owner.clone();
        let on_toggle_routing_combo: Arc<
            dyn Fn(InspectorRoutingCombo, Option<OverlayAnchor>, &mut Window, &mut App) + 'static,
        > = Arc::new(
            move |combo: InspectorRoutingCombo, anchor: Option<OverlayAnchor>, _w, cx| {
                StudioLayout::defer_update(&owner_routing_combo, cx, move |this, cx| {
                    if this.overlay.inspector_routing_combo == Some(combo) {
                        this.overlay.inspector_routing_combo = None;
                        this.overlay.inspector_routing_combo_anchor = None;
                    } else {
                        this.overlay.inspector_routing_combo = Some(combo);
                        this.overlay.inspector_routing_combo_anchor = anchor;
                    }
                    cx.notify();
                });
            },
        );

        // Track colour is committed by `StudioLayout::apply_inspector_color`,
        // driven by the shared colour picker the Inspector renders — there is
        // no separate palette callback here any more.

        InspectorCallbacks {
            on_volume,
            on_volume_drag_start,
            on_volume_drag_preview,
            on_volume_drag_commit,
            on_toggle_volume_automation_read,
            on_pan,
            on_pan_drag_start,
            on_pan_drag_preview,
            on_pan_drag_commit,
            on_toggle_mute,
            on_toggle_solo,
            on_toggle_arm,
            on_toggle_input,
            on_set_input_routing,
            on_set_output_routing,
            on_set_audio_format,
            on_set_midi_input,
            on_set_midi_channel,
            on_set_mpe_configuration,
            on_open_insert_picker,
            on_remove_insert,
            on_toggle_insert_bypass,
            on_toggle_insert_enabled,
            on_toggle_insert_output_channel,
            on_drop_insert,
            on_open_insert_editor,
            on_set_clip_start,
            on_set_clip_length,
            on_set_clip_gain,
            on_set_clip_muted,
            on_set_clip_stretch,
            on_preview_clip_stretch,
            on_begin_clip_property_drag,
            on_commit_clip_property_drag,
            on_preview_clip_start,
            on_preview_clip_length,
            on_preview_clip_gain,
            on_clip_stretch_auto_find_bpm,
            on_clip_warp_clear,
            on_open_clip_bottom_editor,
            on_open_clip_external_midi_editor,
            on_open_soundfont_player,
            open_routing_combo,
            on_toggle_routing_combo,
        }
    }

    fn set_clip_start_cb(&self, owner: Entity<Self>) -> ClipF32Cb {
        let timeline = self.timeline.clone();
        Arc::new(move |(clip_id, start): &(String, f32), _w, cx| {
            let clip_id = clip_id.clone();
            let start = *start;
            let changed = timeline.update(cx, |timeline, cx| {
                let Some(previous) =
                    crate::components::edit::ClipSnapshot::capture(&timeline.state, &clip_id)
                else {
                    return false;
                };
                if !timeline.state.set_clip_start(&clip_id, start) {
                    return false;
                }
                let Some(next) =
                    crate::components::edit::ClipSnapshot::capture(&timeline.state, &clip_id)
                else {
                    return false;
                };
                timeline.record_executed_command(EditCommand::UpdateClip { previous, next }, cx);
                true
            });
            if changed {
                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    this.mark_engine_media_dirty();
                    this.schedule_audio_project_sync(cx, false, "inspector_clip_start");
                    cx.notify();
                });
            }
        })
    }

    fn set_clip_length_cb(&self, owner: Entity<Self>) -> ClipF32Cb {
        let timeline = self.timeline.clone();
        Arc::new(move |(clip_id, length): &(String, f32), _w, cx| {
            let clip_id = clip_id.clone();
            let length = *length;
            let changed = timeline.update(cx, |t, cx| {
                let Some(previous) =
                    crate::components::edit::ClipSnapshot::capture(&t.state, &clip_id)
                else {
                    return false;
                };
                // Inspector Length = trim (crop/reveal source), not stretch.
                if !t.state.set_clip_length_trimming(&clip_id, length) {
                    return false;
                }
                let Some(next) = crate::components::edit::ClipSnapshot::capture(&t.state, &clip_id)
                else {
                    return false;
                };
                t.record_executed_command(EditCommand::UpdateClip { previous, next }, cx);
                true
            });
            if changed {
                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    this.mark_engine_media_dirty();
                    this.schedule_audio_project_sync(cx, false, "inspector_clip_length");
                    cx.notify();
                });
            }
        })
    }

    fn set_clip_gain_cb(&self, owner: Entity<Self>) -> ClipF32Cb {
        let timeline = self.timeline.clone();
        Arc::new(move |(clip_id, gain): &(String, f32), _w, cx| {
            let clip_id = clip_id.clone();
            let gain = *gain;
            let changed = timeline.update(cx, |t, cx| {
                let Some(previous) =
                    crate::components::edit::ClipSnapshot::capture(&t.state, &clip_id)
                else {
                    return false;
                };
                if !t.state.set_clip_gain(&clip_id, gain) {
                    return false;
                }
                let Some(next) = crate::components::edit::ClipSnapshot::capture(&t.state, &clip_id)
                else {
                    return false;
                };
                t.record_executed_command(EditCommand::UpdateClip { previous, next }, cx);
                true
            });
            if changed {
                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    this.mark_engine_media_dirty();
                    this.schedule_audio_project_sync(cx, false, "inspector_clip_gain");
                    cx.notify();
                });
            }
        })
    }

    fn set_clip_muted_cb(&self, owner: Entity<Self>) -> ClipBoolCb {
        let timeline = self.timeline.clone();
        Arc::new(move |(clip_id, muted): &(String, bool), _w, cx| {
            let clip_id = clip_id.clone();
            let muted = *muted;
            let changed = timeline.update(cx, |t, cx| {
                let Some(previous) =
                    crate::components::edit::ClipSnapshot::capture(&t.state, &clip_id)
                else {
                    return false;
                };
                if !t.state.set_clip_muted(&clip_id, muted) {
                    return false;
                }
                let Some(next) = crate::components::edit::ClipSnapshot::capture(&t.state, &clip_id)
                else {
                    return false;
                };
                t.record_executed_command(EditCommand::UpdateClip { previous, next }, cx);
                true
            });
            if changed {
                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    this.mark_engine_media_dirty();
                    this.schedule_audio_project_sync(cx, false, "inspector_clip_muted");
                    cx.notify();
                });
            }
        })
    }

    fn preview_clip_stretch_cb(&self) -> ClipStretchCb {
        let timeline = self.timeline.clone();
        Arc::new(
            move |(clip_id, next): &(String, AudioClipStretchState), _w, cx| {
                let clip_id = clip_id.clone();
                let next = next.clone();
                timeline.update(cx, |timeline, cx| {
                    let project_bpm = timeline.state.bpm as f64;
                    let Some(prev) = timeline.state.clip_stretch(&clip_id).cloned() else {
                        return;
                    };
                    if prev == next {
                        return;
                    }
                    let prev_len = timeline.state.clip_duration_beats(&clip_id).unwrap_or(0.0);
                    let old_ratio = prev.effective_time_ratio(project_bpm);
                    let new_ratio = next.effective_time_ratio(project_bpm);
                    let explicit_next_len = next.clip_timeline_duration_beats;
                    let next_len = if explicit_next_len > 0.0 {
                        explicit_next_len as f32
                    } else if old_ratio > 1e-6 && (old_ratio - new_ratio).abs() > 1e-9 {
                        (prev_len as f64 * (new_ratio / old_ratio)) as f32
                    } else {
                        prev_len
                    };
                    timeline.state.set_clip_stretch(&clip_id, next);
                    if (next_len - prev_len).abs() > 1e-4 {
                        timeline.state.set_clip_length(&clip_id, next_len);
                    }
                    cx.notify();
                });
            },
        )
    }

    /// Apply a full replacement of a clip's stretch/pitch state as one undo
    /// entry. The inspector builds the mutated `AudioClipStretchState`; here we
    /// snapshot the previous value, apply, and record a reversible command.
    fn set_clip_stretch_cb(&self, owner: Entity<Self>) -> ClipStretchCb {
        let timeline = self.timeline.clone();
        Arc::new(
            move |(clip_id, next): &(String, AudioClipStretchState), _w, cx| {
                let clip_id = clip_id.clone();
                let next = next.clone();
                let changed = timeline.update(cx, |t, cx| {
                    let project_bpm = t.state.bpm as f64;
                    let Some(prev) = t.state.clip_stretch(&clip_id).cloned() else {
                        return false;
                    };
                    if prev == next {
                        return false;
                    }
                    let changed_field = stretch_commit_field(&prev, &next);
                    let prev_len = t.state.clip_duration_beats(&clip_id).unwrap_or(0.0);
                    // Couple the clip's timeline length to the time-stretch ratio
                    // so the visual, audible, and exported lengths stay equal
                    // (spec §10). Only the ratio component scales length; toggles
                    // like reverse/fade leave it unchanged.
                    let old_ratio = prev.effective_time_ratio(project_bpm);
                    let new_ratio = next.effective_time_ratio(project_bpm);
                    let explicit_next_len = next.clip_timeline_duration_beats;
                    let next_len = if explicit_next_len > 0.0 {
                        explicit_next_len as f32
                    } else if old_ratio > 1e-6 && (old_ratio - new_ratio).abs() > 1e-9 {
                        (prev_len as f64 * (new_ratio / old_ratio)) as f32
                    } else {
                        prev_len
                    };
                    if clip_dsp_debug_enabled() {
                        eprintln!(
                            "[clip-dsp][inspector-commit] clip_id={} field={} old_ratio={:.3} new_ratio={:.3} old_duration={:.3} new_duration={:.3} snapshot_rebuild=true speed_ratio={:.6} pitch_shift={:+.2} pitch_ratio={:.4} preserve_pitch={}",
                            clip_id,
                            changed_field,
                            old_ratio,
                            new_ratio,
                            prev_len,
                            next_len,
                            next.resample_speed_ratio(project_bpm),
                            next.pitch_shift_semitones,
                            AudioClipStretchState::pitch_ratio_from_semitones(
                                next.pitch_shift_semitones
                            ),
                            next.preserve_pitch,
                        );
                    }
                    t.state.set_clip_stretch(&clip_id, next.clone());
                    if (next_len - prev_len).abs() > 1e-4 {
                        t.state.set_clip_length(&clip_id, next_len);
                    }
                    t.record_executed_command(
                        EditCommand::SetClipStretch {
                            clip_id: clip_id.clone(),
                            prev,
                            next: next.clone(),
                            prev_duration_beats: prev_len,
                            next_duration_beats: next_len,
                        },
                        cx,
                    );
                    true
                });
                if changed {
                    inspector_debug(&format!(
                        "clip stretch clip={clip_id} mode={:?} ratio={:.3}",
                        next.mode, next.stretch_ratio
                    ));
                    StudioLayout::defer_update(&owner, cx, |this, cx| {
                        this.mark_dirty();
                        this.mark_engine_media_dirty();
                        this.schedule_audio_project_sync(cx, false, "inspector_clip_stretch");
                        cx.notify();
                    });
                }
            },
        )
    }

    fn clip_stretch_auto_find_cb(&self, owner: Entity<Self>) -> StrCb {
        Arc::new(move |clip_id: &String, _w, cx| {
            let clip_id = clip_id.clone();
            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                this.stretch_tempo.clear_error(&clip_id);
                this.spawn_clip_tempo_detection(&clip_id, false, cx);
            });
        })
    }

    /// Remove every warp marker from a clip (one undo entry).
    fn clip_warp_clear_cb(&self, owner: Entity<Self>) -> StrCb {
        let timeline = self.timeline.clone();
        Arc::new(move |clip_id: &String, _w, cx| {
            let clip_id = clip_id.clone();
            let changed = timeline.update(cx, |t, cx| {
                let Some(prev) = t.state.clip_stretch(&clip_id).cloned() else {
                    return false;
                };
                if prev.warp_markers.is_empty() {
                    return false;
                }
                let mut next = prev.clone();
                next.warp_markers.clear();
                next.dirty = true;
                let len = t.state.clip_duration_beats(&clip_id).unwrap_or(0.0);
                t.state.set_clip_stretch(&clip_id, next.clone());
                t.record_executed_command(
                    EditCommand::SetClipStretch {
                        clip_id: clip_id.clone(),
                        prev,
                        next,
                        prev_duration_beats: len,
                        next_duration_beats: len,
                    },
                    cx,
                );
                true
            });
            if changed {
                inspector_debug(&format!("clip warp clear clip={clip_id}"));
                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    this.mark_dirty();
                    this.mark_engine_media_dirty();
                    this.schedule_audio_project_sync(cx, false, "inspector_clip_warp_clear");
                    cx.notify();
                });
            }
        })
    }

    fn open_clip_bottom_editor_cb(&self, owner: Entity<Self>) -> StrCb {
        Arc::new(move |clip_id: &String, _w, cx| {
            let clip_id = clip_id.clone();
            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                let _ = this.timeline.update(cx, |timeline, cx| {
                    timeline.state.select_clip(&clip_id);
                    cx.notify();
                });
                inspector_debug(&format!("open_midi_bottom_editor clip={clip_id}"));
                this.open_midi_editor_bottom_panel(cx);
            });
        })
    }

    fn open_clip_external_midi_editor_cb(&self, owner: Entity<Self>) -> StrCb {
        Arc::new(move |clip_id: &String, window, cx| {
            let clip_id = clip_id.clone();
            let bounds = window.bounds();
            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                let _ = this.timeline.update(cx, |timeline, cx| {
                    timeline.state.select_clip(&clip_id);
                    cx.notify();
                });
                inspector_debug(&format!("open_midi_editor clip={clip_id}"));
                this.open_midi_editor_external_window(Some(bounds), cx);
            });
        })
    }

    /// Opens (or focuses) the built-in Soundfont Player MDI window for the
    /// Instrument track whose `builtin_soundfont_player` marker is set. See
    /// [`InspectorCallbacks::on_open_soundfont_player`].
    fn open_soundfont_player_cb(&self, owner: Entity<Self>) -> StrCb {
        Arc::new(move |track_id: &String, window, cx| {
            let track_id = track_id.clone();
            let bounds = window.bounds();
            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                let _ = this.timeline.update(cx, |timeline, cx| {
                    timeline.state.select_track(&track_id);
                    cx.notify();
                });
                inspector_debug(&format!("open_soundfont_player track={track_id}"));
                this.open_soundfont_player_window(Some(bounds), track_id, cx);
            });
        })
    }

    fn insert_picker_cb(&self, owner: Entity<Self>) -> InsertPickerCb {
        Arc::new(
            move |(track_id, slot_index, instrument): &(String, usize, bool), window, cx| {
                let track_id = track_id.clone();
                let slot_index = *slot_index;
                let desired_kind = if *instrument {
                    PluginInsertKind::Instrument
                } else {
                    PluginInsertKind::Effect
                };
                StudioLayout::defer_update_in_window(
                    &owner,
                    window,
                    cx,
                    move |this, window, cx| {
                        inspector_debug(&format!(
                            "insert picker track={track_id} slot={slot_index} kind={desired_kind:?}"
                        ));
                        this.open_insert_picker_for(
                            &track_id,
                            Some(slot_index),
                            desired_kind,
                            Some(window),
                            cx,
                        );
                    },
                );
            },
        )
    }

    fn remove_insert_cb(&self, owner: Entity<Self>) -> InsertPairCb {
        Arc::new(move |(track_id, insert_id): &(String, String), _w, cx| {
            let track_id = track_id.clone();
            let insert_id = insert_id.clone();
            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                // Full RemoveInstrumentPlugin lifecycle: close editor, unload the
                // bridge-host instance, remove the engine sink, drop the slot,
                // re-sync the engine, and assert the instance is gone everywhere.
                this.remove_insert_fully(&track_id, &insert_id, cx, "inspector_remove_insert");
                inspector_debug(&format!(
                    "insert remove track={track_id} insert={insert_id}"
                ));
                this.push_mixer_snapshot_to_window(cx);
                cx.notify();
            });
        })
    }

    fn toggle_insert_bypass_cb(&self, owner: Entity<Self>) -> InsertPairCb {
        Arc::new(move |(track_id, insert_id): &(String, String), _w, cx| {
            let track_id = track_id.clone();
            let insert_id = insert_id.clone();
            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                let edit = this.begin_track_edit(TrackEditScope::channel(&track_id), cx);
                let bypassed = this.timeline.update(cx, |timeline, cx| {
                    let bypassed = timeline
                        .state
                        .toggle_insert_bypass(&track_id, &insert_id)
                        .unwrap_or(false);
                    cx.notify();
                    bypassed
                });
                this.commit_track_edit("Bypass Plug-in", edit, cx);
                inspector_debug(&format!(
                    "insert bypass track={track_id} insert={insert_id} bypass={bypassed}"
                ));
                // Live per-block bypass via the runtime "enabled" insert param —
                // no plugin unload, no graph rebuild (a full `load_project` here
                // used to stutter playback). View-only dirty persists the flag.
                this.push_insert_enabled_to_engine(&track_id, &insert_id, cx);
                this.mark_dirty_view_only();
                this.push_mixer_snapshot_to_window(cx);
                cx.notify();
            });
        })
    }

    fn toggle_insert_enabled_cb(&self, owner: Entity<Self>) -> InsertPairCb {
        Arc::new(move |(track_id, insert_id): &(String, String), _w, cx| {
            let track_id = track_id.clone();
            let insert_id = insert_id.clone();
            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                let edit = this.begin_track_edit(TrackEditScope::channel(&track_id), cx);
                let enabled = this.timeline.update(cx, |timeline, cx| {
                    let enabled = timeline
                        .state
                        .toggle_insert_enabled(&track_id, &insert_id)
                        .unwrap_or(false);
                    cx.notify();
                    enabled
                });
                this.commit_track_edit("Enable Plug-in", edit, cx);
                inspector_debug(&format!(
                    "insert enabled track={track_id} insert={insert_id} enabled={enabled}"
                ));
                // Same live path as bypass: runtime "enabled" param, no rebuild.
                this.push_insert_enabled_to_engine(&track_id, &insert_id, cx);
                this.mark_dirty_view_only();
                this.push_mixer_snapshot_to_window(cx);
                cx.notify();
            });
        })
    }

    fn toggle_insert_output_channel_cb(&self, owner: Entity<Self>) -> InsertOutputChannelCb {
        Arc::new(
            move |(track_id, insert_id, channel, enabled): &(String, String, u8, bool), _w, cx| {
                let track_id = track_id.clone();
                let insert_id = insert_id.clone();
                let channel = *channel;
                let enabled = *enabled;
                StudioLayout::defer_update(&owner, cx, move |this, cx| {
                    // Unticking an output removes its mixer channel: the scope
                    // takes the instrument's output channels in with the track.
                    let edit = this.begin_track_edit(this.channel_with_outputs(&track_id, cx), cx);
                    let changed = this.timeline.update(cx, |timeline, cx| {
                        let ensure_args: Option<(String, u32, bool)> = {
                            let Some(slots) = timeline.state.insert_slots_mut(&track_id) else {
                                return false;
                            };
                            let Some(slot) = slots.iter_mut().find(|slot| slot.id == insert_id)
                            else {
                                return false;
                            };

                            let mut channels = if slot.enabled_audio_output_channels.is_empty() {
                                vec![1, 2]
                            } else {
                                slot.enabled_audio_output_channels.clone()
                            };
                            // Route per OUTPUT BUS, not per single channel: enabling
                            // an output sends its whole stereo pair so one tick gives
                            // full (left+right) sound. A mono bus reports (ch, ch),
                            // so it routes as a duplicated stereo pair. This is the
                            // auto-detect the user expects instead of having to tick
                            // both channels of every pair.
                            let (pair_l, pair_r) = vsti_output_bus_pair_for_channel(
                                &slot.output_bus_channel_counts,
                                channel,
                            );
                            if enabled {
                                channels.push(pair_l);
                                channels.push(pair_r);
                            } else {
                                for ch in [pair_l, pair_r] {
                                    if ch > 2 {
                                        channels.retain(|c| *c != ch);
                                    }
                                }
                            }
                            if !channels.contains(&1) {
                                channels.push(1);
                            }
                            if !channels.contains(&2) {
                                channels.push(2);
                            }
                            channels.retain(|ch| (1..=32).contains(ch));
                            channels.sort_unstable();
                            channels.dedup();

                            if slot.enabled_audio_output_channels == channels {
                                return false;
                            }
                            slot.enabled_audio_output_channels = channels;
                            let plugin_name = slot.display_name.clone();
                            let output_channels =
                                slot.enabled_audio_output_channels
                                    .iter()
                                    .copied()
                                    .max()
                                    .unwrap_or(2) as u32;
                            let multiout_capable =
                                !vsti_output_bus_strip_indices(&slot.output_bus_channel_counts)
                                    .is_empty();
                            Some((plugin_name, output_channels, multiout_capable))
                        };
                        let Some((plugin_name, output_channels, multiout_capable)) = ensure_args
                        else {
                            return false;
                        };
                        timeline.state.ensure_vsti_output_child_tracks(
                            &track_id,
                            &insert_id,
                            output_channels,
                            &plugin_name,
                            multiout_capable,
                        );
                        cx.notify();
                        true
                    });
                    this.commit_track_edit("Instrument Outputs", edit, cx);
                    if changed {
                        inspector_debug(&format!(
                            "insert output channel track={track_id} insert={insert_id} channel={channel} enabled={enabled}"
                        ));
                        this.mark_dirty();
                        this.audio_bridge.project_dirty = true;
                        this.schedule_audio_project_sync(
                            cx,
                            false,
                            "inspector_vsti_output_channel",
                        );
                        this.push_mixer_snapshot_to_window(cx);
                        cx.notify();
                    }
                });
            },
        )
    }

    /// Drop commit for a dragged slot: a reorder within the chain, or a plug-in
    /// dragged in from a docked-mixer strip. Shares `commit_insert_drop` with
    /// every mixer, so one drag is one undo entry wherever it lands and the
    /// drop is resolved against the live chains.
    fn drop_insert_cb(&self, owner: Entity<Self>) -> InsertDropCb {
        Arc::new(move |drop: &InsertDrop, _w, cx| {
            let drop = drop.clone();
            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                if this.commit_insert_drop(&drop, cx) {
                    inspector_debug(&format!(
                        "insert drop from={} insert={} to={} anchor={:?}",
                        drop.from_track, drop.insert_id, drop.to_track, drop.anchor
                    ));
                }
            });
        })
    }

    fn open_insert_editor_cb(&self, owner: Entity<Self>) -> InsertOpenCb {
        Arc::new(
            move |(track_id, insert_index, insert_id): &(String, usize, String), window, cx| {
                let track_id = track_id.clone();
                let insert_index = *insert_index;
                let insert_id = insert_id.clone();
                StudioLayout::defer_update_in_window(
                    &owner,
                    window,
                    cx,
                    move |this, window, cx| {
                        inspector_debug(&format!(
                            "insert open_editor track={track_id} index={insert_index} insert={insert_id}"
                        ));
                        this.open_insert_editor(&track_id, insert_index, &insert_id, window, cx);
                    },
                );
            },
        )
    }

    fn input_routing_cb(&self, owner: Entity<Self>) -> InputRoutingCb {
        use crate::input_routing::TrackInputSelection;

        let audio_engine = self.audio_bridge.engine.clone();
        let timeline = self.timeline.clone();
        Arc::new(
            move |(id, selection): &(String, TrackInputSelection), _w, cx| {
                let id = id.clone();
                // The selector offers logical connections only, so the value
                // written to the track is an AudioConnectionId or nothing —
                // no device, no channel, and no bus created as a side effect.
                let assignment = match selection {
                    TrackInputSelection::OpenAudioConnections => {
                        StudioLayout::defer_update(&owner, cx, |this, cx| {
                            this.open_audio_connections_window(None, cx);
                            cx.notify();
                        });
                        return;
                    }
                    TrackInputSelection::NoInput => None,
                    TrackInputSelection::Connection(connection_id) => Some(connection_id.clone()),
                };
                let old = timeline
                    .read(cx)
                    .state
                    .find_track(&id)
                    .and_then(|track| track.routing.audio_input_connection_id.clone());
                let edit = timeline
                    .read(cx)
                    .begin_track_edit(TrackEditScope::tracks([id.clone()]));
                let changed = timeline.update(cx, |t, cx| {
                    let changed = t
                        .state
                        .set_track_audio_input_connection(&id, assignment.clone());
                    if changed {
                        cx.notify();
                    }
                    changed
                });
                if !changed {
                    return;
                }

                inspector_debug(&format!(
                    "routing audio input track={id} old={old:?} new={assignment:?}"
                ));
                let connections = timeline.read(cx).state.audio_connections.clone();
                let apply_error = audio_engine.as_ref().and_then(|engine| {
                    timeline.read(cx).state.find_track(&id).and_then(|track| {
                        apply_engine_track_input_state(engine, track, &connections).err()
                    })
                });
                if let Some(error) = apply_error {
                    timeline.update(cx, |t, cx| {
                        t.state.set_track_audio_input_connection(&id, old.clone());
                        cx.notify();
                    });
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        this.audio_bridge.last_error =
                            Some(format!("Input routing update failed: {error}"));
                        cx.notify();
                    });
                    return;
                }
                timeline.update(cx, |t, cx| {
                    t.commit_track_edit("Set Input", edit, false, cx);
                });

                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    // A track's input is project data, so this is a real edit.
                    this.mark_dirty();
                    this.publish_audio_connection_routing(cx);
                    cx.notify();
                });
            },
        )
    }

    fn output_routing_cb(&self, owner: Entity<Self>) -> OutputRoutingCb {
        let timeline = self.timeline.clone();
        Arc::new(move |(id, output): &(String, TrackOutputRouting), _w, cx| {
            let id = id.clone();
            let output = output.clone();
            let old = timeline
                .read(cx)
                .state
                .find_track(&id)
                .map(|track| track.routing.output.clone());
            let changed = timeline.update(cx, |t, cx| {
                let edit = t.begin_track_edit(TrackEditScope::tracks([id.clone()]));
                let changed = t.state.set_track_output_routing(&id, output.clone());
                if changed {
                    t.commit_track_edit("Set Output", edit, false, cx);
                    cx.notify();
                }
                changed
            });
            if changed {
                inspector_debug(&format!(
                    "routing output track={id} old={:?} new={:?}",
                    old, output
                ));
                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    this.mark_dirty();
                    this.push_mixer_snapshot_to_window(cx);
                });
            }
        })
    }

    fn audio_format_cb(&self, owner: Entity<Self>) -> AudioFormatCb {
        let audio_engine = self.audio_bridge.engine.clone();
        let timeline = self.timeline.clone();
        Arc::new(
            move |(id, audio_format): &(String, TrackAudioFormat), _w, cx| {
                let id = id.clone();
                let audio_format = *audio_format;
                let previous = timeline.read(cx).state.find_track(&id).map(|track| {
                    (
                        track.routing.audio_format,
                        track.routing.audio_input_connection_id.clone(),
                    )
                });
                let edit = timeline
                    .read(cx)
                    .begin_track_edit(TrackEditScope::tracks([id.clone()]));
                let changed = timeline.update(cx, |t, cx| {
                    let changed = t.state.set_track_audio_format(&id, audio_format);
                    if changed {
                        cx.notify();
                    }
                    changed
                });
                if !changed {
                    return;
                }

                inspector_debug(&format!(
                    "routing audio_format track={id} old={:?} new={:?}",
                    previous.as_ref().map(|(format, _)| *format),
                    audio_format
                ));
                let connections = timeline.read(cx).state.audio_connections.clone();
                let apply_error = audio_engine.as_ref().and_then(|engine| {
                    timeline.read(cx).state.find_track(&id).and_then(|track| {
                        apply_engine_track_input_state(engine, track, &connections).err()
                    })
                });
                if let Some(error) = apply_error {
                    if let Some((old_format, old_input)) = previous {
                        timeline.update(cx, |t, cx| {
                            t.state.set_track_audio_format(&id, old_format);
                            t.state.set_track_audio_input_connection(&id, old_input);
                            cx.notify();
                        });
                    }
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        this.audio_bridge.last_error =
                            Some(format!("Audio format update failed: {error}"));
                        cx.notify();
                    });
                    return;
                }
                timeline.update(cx, |t, cx| {
                    t.commit_track_edit("Set Audio Format", edit, false, cx);
                });

                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    this.mark_dirty();
                    this.push_mixer_snapshot_to_window(cx);
                });
            },
        )
    }

    fn midi_input_cb(&self, owner: Entity<Self>) -> MidiInputCb {
        let timeline = self.timeline.clone();
        Arc::new(
            move |(id, midi_input): &(String, TrackMidiInputRouting), _w, cx| {
                let id = id.clone();
                let midi_input = midi_input.clone();
                let old = timeline
                    .read(cx)
                    .state
                    .find_track(&id)
                    .map(|track| track.routing.midi_input.clone());
                let changed = timeline.update(cx, |t, cx| {
                    let edit = t.begin_track_edit(TrackEditScope::tracks([id.clone()]));
                    let changed = t.state.set_track_midi_input(&id, midi_input.clone());
                    if changed {
                        t.commit_track_edit("Set MIDI Input", edit, false, cx);
                        cx.notify();
                    }
                    changed
                });
                if changed {
                    inspector_debug(&format!(
                        "routing midi_input track={id} old={:?} new={:?}",
                        old, midi_input
                    ));
                    StudioLayout::defer_update(&owner, cx, |this, cx| {
                        this.mark_dirty();
                        cx.notify();
                    });
                }
            },
        )
    }

    fn midi_channel_cb(&self, owner: Entity<Self>) -> MidiChannelCb {
        let timeline = self.timeline.clone();
        Arc::new(move |(id, channel): &(String, Option<u8>), _w, cx| {
            let id = id.clone();
            let channel = *channel;
            let old = timeline
                .read(cx)
                .state
                .find_track(&id)
                .map(|track| track.routing.midi_channel);
            let changed = timeline.update(cx, |t, cx| {
                let edit = t.begin_track_edit(TrackEditScope::tracks([id.clone()]));
                let changed = t.state.set_track_midi_channel(&id, channel);
                if changed {
                    t.commit_track_edit("Set MIDI Channel", edit, false, cx);
                    cx.notify();
                }
                changed
            });
            if changed {
                inspector_debug(&format!(
                    "routing midi_channel track={id} old={:?} new={:?}",
                    old, channel
                ));
                StudioLayout::defer_update(&owner, cx, |this, cx| {
                    this.mark_dirty();
                    this.push_mixer_snapshot_to_window(cx);
                });
            }
        })
    }

    fn mpe_configuration_cb(&self, owner: Entity<Self>) -> MpeConfigurationCb {
        let timeline = self.timeline.clone();
        Arc::new(
            move |(id, configuration): &(String, MpeTrackConfiguration), _w, cx| {
                let id = id.clone();
                let next = configuration.sanitized();
                let changed = timeline.update(cx, |timeline, cx| {
                    let Some(prev) = timeline
                        .state
                        .find_track(&id)
                        .map(|track| track.routing.mpe)
                    else {
                        return false;
                    };
                    if prev == next {
                        return false;
                    }
                    timeline.run_edit_command(
                        EditCommand::SetTrackMpeConfiguration {
                            track_id: id.clone(),
                            prev,
                            next,
                        },
                        cx,
                    );
                    true
                });
                if changed {
                    inspector_debug(&format!("routing mpe track={id} mode={:?}", next.mode));
                    StudioLayout::defer_update(&owner, cx, |this, cx| {
                        this.mark_dirty();
                        this.schedule_audio_project_sync(cx, true, "inspector_mpe_configuration");
                        this.push_mixer_snapshot_to_window(cx);
                    });
                }
            },
        )
    }

    /// Build one of the four M/S/R/I toggle callbacks. Every toggle is persisted
    /// without marking the project graph dirty; arm/monitor use the dedicated
    /// input-state command while mute/solo keep their existing realtime params.
    fn track_toggle_cb(&self, owner: Entity<Self>, kind: TrackToggle) -> StrCb {
        let audio_engine = self.audio_bridge.engine.clone();
        let timeline = self.timeline.clone();
        Arc::new(move |id: &String, _w, cx: &mut App| {
            let id = id.clone();
            let previous_input_state = timeline
                .read(cx)
                .state
                .find_track(&id)
                .map(|track| (track.armed, track.input_monitor));
            let edit = timeline
                .read(cx)
                .begin_track_edit(TrackEditScope::tracks([id.clone()]));
            let mut value = false;
            let changed = timeline.update(cx, |t, cx| {
                let changed = match kind {
                    TrackToggle::Mute => t.state.toggle_track_mute(&id),
                    TrackToggle::Solo => t.state.toggle_track_solo(&id),
                    TrackToggle::Arm => t.state.toggle_track_arm(&id),
                    TrackToggle::Input => t.state.cycle_track_input_monitor(&id),
                };
                value = t
                    .state
                    .find_track(&id)
                    .map(|track| match kind {
                        TrackToggle::Mute => track.muted,
                        TrackToggle::Solo => track.solo,
                        TrackToggle::Arm => track.armed,
                        TrackToggle::Input => track.input_monitor.is_active(track.armed),
                    })
                    .unwrap_or(false);
                if changed {
                    cx.notify();
                }
                changed
            });
            if !changed {
                return;
            }
            inspector_debug(&format!(
                "edit track {} track={id} new={value}",
                kind.label()
            ));

            if matches!(kind, TrackToggle::Arm | TrackToggle::Input) {
                let connections = timeline.read(cx).state.audio_connections.clone();
                let apply_error = audio_engine.as_ref().and_then(|engine| {
                    timeline.read(cx).state.find_track(&id).and_then(|track| {
                        apply_engine_track_input_state(engine, track, &connections).err()
                    })
                });
                if let Some(error) = apply_error {
                    if let Some((armed, input_monitor)) = previous_input_state {
                        timeline.update(cx, |t, cx| {
                            if let Some(track) =
                                t.state.tracks.iter_mut().find(|track| track.id == id)
                            {
                                track.armed = armed;
                                track.input_monitor = input_monitor;
                            }
                            cx.notify();
                        });
                    }
                    StudioLayout::defer_update(&owner, cx, move |this, cx| {
                        this.audio_bridge.last_error =
                            Some(format!("Track input update failed: {error}"));
                        this.push_mixer_snapshot_to_window(cx);
                    });
                    return;
                }
            }
            timeline.update(cx, |t, cx| {
                t.commit_track_edit(kind.history_label(), edit, false, cx);
            });

            StudioLayout::defer_update(&owner, cx, move |this, cx| {
                this.mark_dirty_view_only();
                this.push_mixer_snapshot_to_window(cx);
            });
            if let Some(engine) = audio_engine.as_ref() {
                let param = match kind {
                    TrackToggle::Mute => Some("muted"),
                    TrackToggle::Solo => Some("solo"),
                    TrackToggle::Arm | TrackToggle::Input => None,
                };
                if let Some(param) = param {
                    let _ = engine.update_track_param(&id, param, if value { 1.0 } else { 0.0 });
                }
            }
        })
    }
}

#[derive(Clone, Copy)]
enum TrackToggle {
    Mute,
    Solo,
    Arm,
    Input,
}

impl TrackToggle {
    fn label(self) -> &'static str {
        match self {
            TrackToggle::Mute => "mute",
            TrackToggle::Solo => "solo",
            TrackToggle::Arm => "arm",
            TrackToggle::Input => "input-monitor",
        }
    }

    /// The toggle as the Edit menu names its undo step.
    fn history_label(self) -> &'static str {
        match self {
            TrackToggle::Mute => "Mute",
            TrackToggle::Solo => "Solo",
            TrackToggle::Arm => "Record Arm",
            TrackToggle::Input => "Input Monitor",
        }
    }
}

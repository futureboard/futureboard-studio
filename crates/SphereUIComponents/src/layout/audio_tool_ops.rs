//! Open/close audio-editor tool windows and apply their commands.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, Context, Window};
use sphere_audio_editor::{AudioToolKind, AudioToolTarget};

use crate::components::timeline::timeline_state::{
    AudioImportState, ClipType, StretchMode, WarpMarker, MIN_AUDIO_CLIP_BEATS,
};
use crate::components::timeline::{waveform_cache, waveform_detail, waveform_samples};
use crate::components::{
    apply_previews_to_snapshot, open_audio_tool_window, AudioToolCommand, AudioToolWindowCallbacks,
};

use super::StudioLayout;

impl StudioLayout {
    pub(super) fn flush_pending_audio_tools(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pending = self
            .audio_editor
            .update(cx, |editor, _cx| editor.take_pending_open_tool());
        if let Some((kind, target)) = pending {
            let owner = cx.entity().clone();
            let owner_bounds = window.bounds();
            cx.defer(move |cx| {
                let _ = owner.update(cx, |layout, cx| {
                    layout.open_audio_tool(kind, target, Some(owner_bounds), cx);
                });
            });
        }

        let audition = self
            .audio_editor
            .update(cx, |editor, _cx| editor.take_pending_audition());
        if let Some((beat, start)) = audition {
            let _ = self.timeline.update(cx, |timeline, cx| {
                timeline.seek_to_exact_beat(beat, crate::layout::SeekReason::TimelineClick, cx);
            });
            if start {
                let playing = self
                    .audio_bridge
                    .stats
                    .as_ref()
                    .map(|stats| stats.transport_playing)
                    .unwrap_or(false);
                if !playing {
                    self.start_native_playback(cx);
                    self.audio_editor_audition_owned = true;
                }
            } else if self.audio_editor_audition_owned {
                self.stop_native_playback(cx);
                self.audio_editor_audition_owned = false;
            }
        }

        if !self.audio_tools.windows.is_empty() {
            if let Some(target) = self
                .audio_editor
                .update(cx, |editor, cx| editor.current_tool_target(cx))
            {
                self.audio_tools.follow_selection(&target, cx);
            }
        }
    }

    pub(crate) fn open_audio_tool(
        &mut self,
        kind: AudioToolKind,
        target: AudioToolTarget,
        owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        self.audio_tools.prune(cx);
        if let Some(handle) = self.audio_tools.windows.get(&kind).cloned() {
            let _ = handle.update(cx, |window, win, cx| {
                if window.session.follow_selection && !window.session.pin_target {
                    window.session.target = target.clone();
                    DirectAudio::analysis_tap().set_target_clip(Some(&target.clip_id));
                }
                win.activate_window();
                cx.notify();
            });
            return;
        }

        let owner_bounds = owner_bounds.or_else(|| self.studio_window_bounds(cx));
        let remembered = self.audio_tools.last_bounds.get(&kind).copied();
        let layout = cx.entity().clone();
        let callbacks = AudioToolWindowCallbacks {
            on_command: {
                let layout = layout.clone();
                Arc::new(move |command, cx: &mut App| {
                    StudioLayout::defer_update(&layout, cx, move |this, cx| {
                        this.handle_audio_tool_command(command, cx);
                    });
                })
            },
            on_close: {
                let layout = layout.clone();
                Arc::new(move |kind, bounds, cx: &mut App| {
                    StudioLayout::defer_update(&layout, cx, move |this, cx| {
                        this.audio_tools.last_bounds.insert(kind, bounds);
                        this.audio_tools.windows.remove(&kind);
                        this.audio_tools.previews.clear();
                        this.mark_engine_media_dirty();
                        this.schedule_audio_project_sync(cx, false, "audio_tool_close");
                    });
                })
            },
        };

        match open_audio_tool_window(
            kind,
            target,
            owner_bounds,
            remembered,
            self.timeline.clone(),
            callbacks,
            cx,
        ) {
            Ok(handle) => {
                self.audio_tools.windows.insert(kind, handle);
            }
            Err(error) => eprintln!("[audio-tool] failed to open {}: {error}", kind.label()),
        }
    }

    pub(super) fn handle_audio_tool_command(
        &mut self,
        command: AudioToolCommand,
        cx: &mut Context<Self>,
    ) {
        match command {
            AudioToolCommand::Preview(preview) => {
                self.audio_tools
                    .previews
                    .insert(preview.clip_id.clone(), preview);
                self.mark_engine_media_dirty();
                self.schedule_audio_project_sync(cx, false, "audio_tool_preview");
            }
            AudioToolCommand::ClearPreview(clip_id) => {
                self.audio_tools.previews.remove(&clip_id);
                self.mark_engine_media_dirty();
                self.schedule_audio_project_sync(cx, false, "audio_tool_preview_clear");
            }
            AudioToolCommand::MutateClip {
                clip_id,
                label: _,
                mutate,
            } => {
                let source_path =
                    self.timeline
                        .read(cx)
                        .state
                        .find_clip(&clip_id)
                        .and_then(|(_, clip)| match &clip.clip_type {
                            ClipType::Audio { source_path, .. } => source_path.clone(),
                            _ => None,
                        });
                let _ = self.timeline.update(cx, |timeline, cx| {
                    timeline.begin_inspector_clip_gesture(&clip_id);
                    let bpm = timeline.state.bpm.max(1.0) as f64;
                    let (old_ratio, old_len) = timeline
                        .state
                        .find_clip(&clip_id)
                        .map(|(_, clip)| {
                            (clip.stretch.effective_time_ratio(bpm), clip.duration_beats)
                        })
                        .unwrap_or((1.0, 0.0));
                    let mut found = false;
                    for track in &mut timeline.state.tracks {
                        if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                            mutate(clip);
                            let new_ratio = clip.stretch.effective_time_ratio(bpm);
                            if old_ratio > 1.0e-6 {
                                let next_len = (old_len as f64 * (new_ratio / old_ratio))
                                    .max(MIN_AUDIO_CLIP_BEATS as f64);
                                if (clip.duration_beats as f64 - next_len).abs() > 1.0e-4 {
                                    clip.duration_beats = next_len as f32;
                                }
                            }
                            found = true;
                            break;
                        }
                    }
                    if found && timeline.commit_inspector_clip_gesture(&clip_id, cx) {
                        timeline.mark_media_changed(cx);
                    }
                });
                self.audio_tools.previews.remove(&clip_id);
                self.refresh_audio_editor_visuals(source_path.as_deref(), cx);
            }
            AudioToolCommand::AddMarkers { beats, label } => {
                let _ = self.timeline.update(cx, |timeline, cx| {
                    let prev = timeline.state.markers.clone();
                    for beat in beats {
                        timeline.state.add_marker_at_beat(beat);
                    }
                    if timeline.record_marker_edit(label, prev, cx) {
                        timeline.mark_media_changed(cx);
                    }
                });
            }
            AudioToolCommand::AddWarpMarkers { clip_id, frames } => {
                let _ = self.timeline.update(cx, |timeline, cx| {
                    timeline.begin_inspector_clip_gesture(&clip_id);
                    let start = timeline
                        .state
                        .find_clip(&clip_id)
                        .map(|(_, clip)| clip.start_beat as f64)
                        .unwrap_or(0.0);
                    let spb = timeline.state.seconds_per_beat() as f64;
                    let sr = timeline
                        .state
                        .find_clip(&clip_id)
                        .map(|(_, clip)| clip.stretch.original_sample_rate.max(1) as f64)
                        .unwrap_or(48_000.0);
                    if let Some(mut stretch) = timeline.state.clip_stretch(&clip_id).cloned() {
                        stretch.warp_markers = frames
                            .into_iter()
                            .enumerate()
                            .map(|(index, source_sample)| WarpMarker {
                                id: index as u64 + 1,
                                source_sample,
                                timeline_beat: start
                                    + (source_sample as f64 / sr) / spb.max(1.0e-6),
                                locked: false,
                            })
                            .collect();
                        stretch.dirty = true;
                        let _ = timeline.state.set_clip_stretch(&clip_id, stretch);
                    }
                    if timeline.commit_inspector_clip_gesture(&clip_id, cx) {
                        timeline.mark_media_changed(cx);
                    }
                });
            }
            AudioToolCommand::SliceClip { clip_id, mut beats } => {
                beats.sort_by(|a, b| b.total_cmp(a));
                let _ = self.timeline.update(cx, |timeline, cx| {
                    let mut current_id = clip_id;
                    for beat in beats {
                        if timeline.split_audio_clip_at_beat(&current_id, beat, cx) {
                            current_id = timeline
                                .state
                                .find_clip(&current_id)
                                .map(|(_, clip)| clip.id.clone())
                                .unwrap_or(current_id);
                        }
                    }
                    timeline.mark_media_changed(cx);
                });
            }
            AudioToolCommand::UseOriginalBpm { clip_id, bpm } => {
                let _ = self.timeline.update(cx, |timeline, cx| {
                    timeline.begin_inspector_clip_gesture(&clip_id);
                    if let Some(mut stretch) = timeline.state.clip_stretch(&clip_id).cloned() {
                        stretch.bpm_source = Some(bpm);
                        stretch.dirty = true;
                        let _ = timeline.state.set_clip_stretch(&clip_id, stretch);
                    }
                    if timeline.commit_inspector_clip_gesture(&clip_id, cx) {
                        timeline.mark_media_changed(cx);
                    }
                });
            }
            AudioToolCommand::AddTempoPoint { beat, bpm } => {
                let _ = self.timeline.update(cx, |timeline, cx| {
                    let prev = timeline.capture_tempo_state();
                    let _ = timeline.state.add_tempo_point(beat, bpm);
                    if timeline.record_tempo_edit("Add Tempo Marker", prev, cx) {
                        timeline.mark_media_changed(cx);
                    }
                });
            }
            AudioToolCommand::ReplaceSource {
                clip_id,
                path,
                sample_rate,
            } => {
                self.replace_clip_source(&clip_id, path, sample_rate, cx);
            }
        }
        cx.notify();
    }

    fn replace_clip_source(
        &mut self,
        clip_id: &str,
        path: PathBuf,
        sample_rate: u32,
        cx: &mut Context<Self>,
    ) {
        let path_string = path.to_string_lossy().into_owned();
        let _ = self.timeline.update(cx, |timeline, cx| {
            timeline.begin_inspector_clip_gesture(clip_id);
            for track in &mut timeline.state.tracks {
                if let Some(clip) = track.clips.iter_mut().find(|clip| clip.id == clip_id) {
                    if let ClipType::Audio {
                        file_id,
                        source_path,
                    } = &mut clip.clip_type
                    {
                        *file_id = path_string.clone();
                        *source_path = Some(path_string.clone());
                    }
                    clip.audio_import = AudioImportState::Pending;
                    clip.source_duration_seconds = None;
                    clip.gain = 1.0;
                    clip.stretch.original_sample_rate = sample_rate;
                    clip.stretch.project_sample_rate = sample_rate;
                    clip.stretch.original_duration_samples = 0;
                    clip.stretch.source_start_samples = 0;
                    clip.stretch.source_end_samples = 0;
                    clip.stretch.mode = StretchMode::Off;
                    clip.stretch.stretch_ratio = 1.0;
                    clip.stretch.reset_pitch();
                    clip.stretch.preserve_pitch = false;
                    clip.stretch.channel_transform = 0;
                    clip.stretch.dc_remove = false;
                    clip.stretch.dehum_hz = 0.0;
                    clip.stretch.denoise_amount = 0.0;
                    clip.stretch.dirty = true;
                    break;
                }
            }
            if timeline.commit_inspector_clip_gesture(clip_id, cx) {
                timeline.mark_media_changed(cx);
            }
        });
        // Only drop peaks for the file that was just rewritten. Sibling clips
        // still pointing at the original source must keep drawing it, and undo
        // must be able to restore that cache instead of an empty waveform.
        waveform_cache::invalidate_file(&path_string);
        waveform_detail::forget_asset(&path_string);
        waveform_samples::forget_asset(&path_string);
        self.audio_tools.previews.remove(clip_id);
        self.spawn_timeline_audio_import_jobs(cx, self.timeline.clone(), path, path_string.clone());
        self.refresh_audio_editor_visuals(Some(&path_string), cx);
    }

    fn refresh_audio_editor_visuals(&mut self, source_path: Option<&str>, cx: &mut Context<Self>) {
        let path = source_path.map(str::to_string);
        let _ = self.audio_editor.update(cx, |editor, cx| {
            editor.refresh_clip_visuals(path.as_deref(), cx);
        });
        let _ = self.clip_editor_panel.update(cx, |_, cx| cx.notify());
    }

    pub(super) fn overlay_audio_tool_previews(
        &self,
        snapshot: &mut DirectAudio::types::EngineProjectSnapshot,
    ) {
        apply_previews_to_snapshot(snapshot, &self.audio_tools.previews);
    }

    pub(super) fn close_audio_tool_windows(&mut self, cx: &mut Context<Self>) {
        self.audio_tools.close_all(cx);
    }
}

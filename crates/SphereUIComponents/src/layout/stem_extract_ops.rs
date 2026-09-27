//! StudioLayout integration for the Stem Extractor dialog.
//!
//! Captures audio clips currently on arrangement tracks (plain owned data),
//! then opens the external Stem Extractor window. On success the layout creates
//! one new audio track per stem (aligned to the source clip), mutes the
//! original source track, and syncs the engine.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Bounds, Context};

use super::StudioLayout;
use crate::components::timeline::timeline_state::{
    volume, ClipType, CreateTrackOptions, InputMonitorMode, TrackType,
};
use crate::components::{
    open_stem_extractor_window, StemExtractApplyRequest, StemExtractorDialogDefaults,
    StemSourceClip,
};

impl StudioLayout {
    pub(super) fn open_stem_extractor_external_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.stem_extractor.clone() {
            if handle
                .update(cx, |_w, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.stem_extractor = None;
        }

        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.overlay.open_popover = None;
        self.overlay.text_context_menu = None;

        let tl_state = self.timeline.read(cx).state.clone();
        let project_name = self.project_session.name.clone();
        let project_root = self
            .project_session
            .folder_path
            .as_ref()
            .map(|p| p.to_path_buf());
        let paths = crate::paths::FutureboardPaths::resolve();
        let _ = paths.ensure_user_dirs();
        let models_dir = Some(paths.models.clone());

        let audio_clips = collect_audio_source_clips(&tl_state);
        let selected_clip_id = tl_state
            .selection
            .selected_clip_ids
            .iter()
            .find(|id| audio_clips.iter().any(|clip| clip.clip_id == **id))
            .cloned()
            .or_else(|| audio_clips.first().map(|clip| clip.clip_id.clone()));

        let defaults = StemExtractorDialogDefaults {
            project_name,
            audio_clips,
            selected_clip_id,
            project_root,
            models_dir,
        };

        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            owner_bounds,
            self.studio_window_bounds(cx),
            cx,
        );

        let layout = cx.entity().clone();
        let on_apply: Arc<dyn Fn(StemExtractApplyRequest, &mut gpui::App) + 'static> =
            Arc::new(move |request, cx| {
                let _ = layout.update(cx, |this, cx| {
                    this.apply_stem_extract_result(request, cx);
                });
            });

        match open_stem_extractor_window(owner_bounds, defaults, on_apply, cx) {
            Ok(handle) => self.external_windows.stem_extractor = Some(handle),
            Err(err) => eprintln!("[stem-extractor] failed to open window: {err}"),
        }
    }

    fn apply_stem_extract_result(
        &mut self,
        request: StemExtractApplyRequest,
        cx: &mut Context<Self>,
    ) {
        if request.stems.is_empty() {
            return;
        }

        let timeline = self.timeline.clone();
        let mut import_jobs: Vec<(PathBuf, String)> = Vec::new();
        let muted_source = self.timeline.update(cx, |timeline, cx| {
            // The stem tracks and the muted source are one step.
            let source_id = request.source_track_id.clone();
            let edit = timeline.begin_track_edit(
                crate::components::timeline::timeline_state::TrackEditScope::tracks([source_id]),
            );
            let source_index = timeline
                .state
                .tracks
                .iter()
                .position(|track| track.id == request.source_track_id)
                .unwrap_or(timeline.state.tracks.len().saturating_sub(1));

            let mut created_ids = Vec::new();
            for (insert_at, stem) in (source_index + 1..).zip(request.stems.iter()) {
                let path_string = stem.path.to_string_lossy().into_owned();
                let track_name = stem.kind.label().to_string();
                let track_id = timeline.state.create_track(CreateTrackOptions {
                    track_type: TrackType::Audio,
                    name: track_name.clone(),
                    color: timeline
                        .state
                        .track_color_for_index(timeline.state.tracks.len()),
                    volume: volume::db_to_norm(0.0),
                    pan: 0.0,
                    armed: false,
                    input_monitor: InputMonitorMode::Off,
                });
                let _ = timeline.state.reorder_track(&track_id, insert_at);

                let clip_name = format!(
                    "{}_{}",
                    sanitize_stem_clip_name(&request.source_clip_name),
                    stem.kind.file_stem_suffix()
                );
                let _ = timeline.state.insert_audio_clip_with_duration(
                    track_id.clone(),
                    path_string.clone(),
                    clip_name,
                    request.start_beat,
                    request.duration_beats,
                    request.source_duration_seconds,
                );
                created_ids.push(track_id);
                import_jobs.push((stem.path.clone(), path_string));
            }

            let muted = timeline
                .state
                .set_track_mute(&request.source_track_id, true);
            if let Some(last) = created_ids.last() {
                timeline.state.select_track(last);
            }
            eprintln!(
                "[stem-extractor] applied stems={} muted_source={} source_track={} created={:?}",
                request.stems.len(),
                muted,
                request.source_track_id,
                created_ids
            );
            timeline.commit_track_edit("Extract Stems", edit, false, cx);
            cx.notify();
            muted.then_some(request.source_track_id.clone())
        });

        if let Some(source_track_id) = muted_source {
            if let Some(engine) = self.audio_bridge.engine.as_ref() {
                let _ = engine.update_track_param(&source_track_id, "muted", 1.0);
            }
        }

        for (path, path_key) in import_jobs {
            self.spawn_timeline_audio_import_jobs(cx, timeline.clone(), path, path_key);
        }

        self.mark_dirty();
        self.mark_engine_media_dirty();
        self.schedule_audio_project_sync(cx, true, "stem_extract_apply");
        self.push_mixer_snapshot_to_window(cx);
        cx.notify();
    }
}

fn collect_audio_source_clips(
    tl_state: &crate::components::timeline::timeline_state::TimelineState,
) -> Vec<StemSourceClip> {
    let mut clips = Vec::new();
    for track in &tl_state.tracks {
        for clip in &track.clips {
            let ClipType::Audio {
                source_path: Some(path),
                ..
            } = &clip.clip_type
            else {
                continue;
            };
            let path = PathBuf::from(path);
            if !path.exists() {
                continue;
            }
            clips.push(StemSourceClip {
                clip_id: clip.id.clone(),
                track_id: track.id.clone(),
                track_name: track.name.clone(),
                clip_name: clip.name.clone(),
                source_path: path,
                start_beat: clip.start_beat,
                duration_beats: clip.duration_beats,
                source_duration_seconds: clip.source_duration_seconds,
            });
        }
    }
    clips
}

fn sanitize_stem_clip_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "stem".into()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{ClipState, ClipType, TimelineState};

    #[test]
    fn collect_only_resolvable_audio_clips() {
        let mut state = TimelineState::default();
        let track_id = state.create_audio_track();
        {
            let track = state
                .tracks
                .iter_mut()
                .find(|track| track.id == track_id)
                .expect("audio track");
            track.name = "Drums".into();
            track.clips.push(ClipState {
                id: "clip-a".into(),
                name: "Loop A".into(),
                start_beat: 0.0,
                duration_beats: 4.0,
                source_duration_seconds: Some(2.0),
                offset_beats: 0.0,
                gain: 1.0,
                clip_type: ClipType::Audio {
                    file_id: "a".into(),
                    source_path: Some("/tmp/does-not-exist-stem.wav".into()),
                },
                muted: false,
                audio_import: Default::default(),
                stretch: Default::default(),
            });
            let existing = std::env::temp_dir().join("fb-stem-source-test.wav");
            std::fs::write(&existing, b"RIFF").unwrap();
            track.clips.push(ClipState {
                id: "clip-b".into(),
                name: "Loop B".into(),
                start_beat: 4.0,
                duration_beats: 4.0,
                source_duration_seconds: Some(2.0),
                offset_beats: 0.0,
                gain: 1.0,
                clip_type: ClipType::Audio {
                    file_id: "b".into(),
                    source_path: Some(existing.display().to_string()),
                },
                muted: false,
                audio_import: Default::default(),
                stretch: Default::default(),
            });
        }

        let clips = collect_audio_source_clips(&state);
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].clip_id, "clip-b");
        assert_eq!(clips[0].label(), "Drums · Loop B");
        assert_eq!(clips[0].start_beat, 4.0);
        let _ = std::fs::remove_file(std::env::temp_dir().join("fb-stem-source-test.wav"));
    }
}

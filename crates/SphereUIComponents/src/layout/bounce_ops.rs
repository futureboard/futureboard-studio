//! Freeze Track, Render in Place, Render Selection, Flatten and Consolidate.
//!
//! Each renders tracks on their own — through their instrument and inserts,
//! before the fader — with the offline exporter on a worker thread, the live
//! plug-in bridges lent to it as Export does, then applies what it wrote as
//! one undo step. The worker never touches GPUI; the studio polls what it
//! shares.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{Context, WindowHandle};
use sphere_encoder::{AudioEncodeOptions, AudioFileFormat, AudioSampleFormat};
use DirectAudio::types::EngineProjectSnapshot;
use DirectAudio::{
    beats_to_samples, export_render_job_with_bridges, render_block_size_for_sample_rate,
    ArrangementExportRequest, ExportCancelToken, ExportNormalizeMode, ExportTailMode,
    OfflineRenderRequest, RenderJob, TrackExportTarget,
};

use super::engine_snapshot::build_engine_project_snapshot_for_export;
use super::StudioLayout;
use crate::components::progress_dialog::{
    open_progress_dialog_window, ProgressBarValue, ProgressDialogOptions, ProgressDialogWindow,
};
use crate::components::timeline::timeline_state::{
    vsti_output_child_insert_id, AutomationTarget, ClipType, CreateTrackOptions, InputMonitorMode,
    TimelineState, TrackEditScope, TrackFreeze, TrackType,
};

/// How long a freeze or whole-track render keeps going after the last clip
/// for a reverb or release to ring out, at most.
const TAIL_MAX_SECONDS: f64 = 10.0;
const TAIL_SILENCE_DB: f32 = -90.0;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BounceKind {
    Freeze,
    RenderInPlace,
    RenderSelection,
    Consolidate,
}

impl BounceKind {
    fn title(self) -> &'static str {
        match self {
            Self::Freeze => "Freeze Track",
            Self::RenderInPlace => "Render in Place",
            Self::RenderSelection => "Render Selection",
            Self::Consolidate => "Consolidate Clips",
        }
    }

    fn tail(self) -> ExportTailMode {
        match self {
            Self::Freeze | Self::RenderInPlace => ExportTailMode::UntilSilence {
                max_seconds: TAIL_MAX_SECONDS,
                threshold_db: TAIL_SILENCE_DB,
            },
            Self::RenderSelection | Self::Consolidate => ExportTailMode::None,
        }
    }

    fn file_suffix(self) -> &'static str {
        match self {
            Self::Freeze => "Freeze",
            Self::RenderInPlace | Self::RenderSelection => "Render",
            Self::Consolidate => "Consolidated",
        }
    }
}

/// One track to render on its own.
#[derive(Debug, Clone)]
struct BounceTarget {
    track_id: String,
    track_name: String,
    start_beat: f64,
    end_beat: f64,
    /// Consolidate: only these clips, played without the track's inserts.
    clip_ids: Option<Vec<String>>,
    output: PathBuf,
}

#[derive(Debug, Clone)]
struct BounceResult {
    target: BounceTarget,
    duration_seconds: f64,
    sample_rate: u32,
}

#[derive(Default)]
struct BounceShared {
    progress: f32,
    done: Option<Result<Vec<BounceResult>, String>>,
}

/// The snapshot a bounce renders: only the targets' own material, each
/// target at unity gain, centred and unmuted with no fader automation, so
/// its tap — which the engine takes after the fader — is the pre-fader
/// signal. Nothing else plays, and no input is monitored.
fn isolate_for_bounce(snapshot: &mut EngineProjectSnapshot, targets: &[BounceTarget]) {
    let target = |id: &str| targets.iter().find(|t| t.track_id == id);
    snapshot.clips.retain(|clip| {
        target(&clip.track_id).is_some_and(|t| {
            t.clip_ids
                .as_ref()
                .is_none_or(|ids| ids.iter().any(|id| *id == clip.id))
        })
    });
    snapshot
        .midi_clips
        .retain(|clip| target(&clip.track_id).is_some_and(|t| t.clip_ids.is_none()));
    let fader_tags = [
        AutomationTarget::TrackVolume.to_tag(),
        AutomationTarget::TrackPan.to_tag(),
        AutomationTarget::TrackMute.to_tag(),
    ];
    for track in &mut snapshot.tracks {
        track.solo = false;
        track.armed = false;
        track.input_monitor = false;
        let Some(t) = target(&track.id) else {
            continue;
        };
        track.volume = 1.0;
        track.pan = 0.0;
        track.muted = false;
        track
            .automation_lanes
            .retain(|lane| !fader_tags.contains(&lane.target.tag));
        if t.clip_ids.is_some() {
            for insert in &mut track.inserts {
                insert.enabled = false;
            }
        }
    }
}

/// `(first beat, last beat)` the track sounds in `snapshot`: its clips, and
/// the MIDI played on its instrument from other tracks.
fn track_content_range(snapshot: &EngineProjectSnapshot, track_id: &str) -> Option<(f64, f64)> {
    let audio = snapshot
        .clips
        .iter()
        .filter(|c| c.track_id == track_id && !c.muted)
        .map(|c| (c.start_beat, c.start_beat + c.duration_beats));
    let midi = snapshot
        .midi_clips
        .iter()
        .filter(|c| c.track_id == track_id)
        .map(|c| (c.start_beat, c.start_beat + c.length_beats));
    audio
        .chain(midi)
        .filter(|(start, end)| end > start)
        .fold(None, |range, (start, end)| match range {
            None => Some((start, end)),
            Some((s, e)) => Some((f64::min(s, start), f64::max(e, end))),
        })
}

fn bounce_request(
    snapshot: &EngineProjectSnapshot,
    target: &BounceTarget,
    sample_rate: u32,
    tail: ExportTailMode,
) -> ArrangementExportRequest {
    ArrangementExportRequest {
        output_path: target.output.clone(),
        format: AudioFileFormat::Wav,
        sample_format: AudioSampleFormat::F32,
        render: OfflineRenderRequest {
            sample_rate,
            channels: 2,
            start_sample: beats_to_samples(snapshot, target.start_beat, sample_rate),
            end_sample: beats_to_samples(snapshot, target.end_beat, sample_rate),
            master_volume: 1.0,
            block_size: render_block_size_for_sample_rate(snapshot, sample_rate),
            tail,
            normalize: ExportNormalizeMode::None,
        },
        encode_options: AudioEncodeOptions {
            format: AudioFileFormat::Wav,
            ..Default::default()
        },
    }
}

/// A file name in `dir` no earlier bounce took.
fn unique_bounce_path(dir: &Path, track_name: &str, suffix: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let stem = format!(
        "{}_{suffix}_{stamp}",
        crate::export::sanitize_file_stem(track_name)
    );
    let mut path = dir.join(format!("{stem}.wav"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem}_{n}.wav"));
        n += 1;
    }
    path
}

/// Where a bounced track goes: right below `source_id`, past the VSTi output
/// channels that belong to it.
fn insert_index_below(state: &TimelineState, source_id: &str) -> usize {
    let Some(index) = state.tracks.iter().position(|t| t.id == source_id) else {
        return state.tracks.len();
    };
    let source = &state.tracks[index];
    let mut at = index + 1;
    while state.tracks.get(at).is_some_and(|track| {
        vsti_output_child_insert_id(&track.id)
            .is_some_and(|insert| source.inserts.iter().any(|slot| slot.id == insert))
    }) {
        at += 1;
    }
    at
}

/// A new audio track below `source_id` playing `path` from `start_beat`, its
/// channel set up like the source's so it sits in the mix where the source
/// did. With `whole_channel` (Flatten) it takes the source's name, mute,
/// solo, sends and fader/pan/send automation too. Returns its id.
#[allow(clippy::too_many_arguments)]
fn add_bounced_track(
    state: &mut TimelineState,
    source_id: &str,
    name: String,
    path: &Path,
    start_beat: f64,
    duration_seconds: f64,
    sample_rate: u32,
    whole_channel: bool,
) -> Option<String> {
    let source = state.find_track(source_id)?.clone();
    let at = insert_index_below(state, source_id);
    let track_id = state.create_track(CreateTrackOptions {
        track_type: TrackType::Audio,
        name: name.clone(),
        color: source.color,
        volume: source.volume,
        pan: source.pan,
        armed: false,
        input_monitor: InputMonitorMode::Off,
    });
    let _ = state.reorder_track(&track_id, at);
    if let Some(track) = state.tracks.iter_mut().find(|t| t.id == track_id) {
        track.parent_group_id = source.parent_group_id.clone();
        track.routing.output = source.routing.output.clone();
        track.spatial = source.spatial;
        track.timebase = source.timebase;
        if whole_channel {
            track.muted = source.muted;
            track.solo = source.solo;
            track.sends = source.sends.clone();
            track.volume_automation_read = source.volume_automation_read;
            track.automation_lanes = source
                .automation_lanes
                .iter()
                .filter(|lane| !matches!(lane.target, AutomationTarget::PluginParameter { .. }))
                .cloned()
                .collect();
        }
    }
    let clip_id = state.insert_recorded_clip(
        &track_id,
        path.to_string_lossy().into_owned(),
        name,
        start_beat.max(0.0) as f32,
        duration_seconds,
        state.bpm,
    );
    state.seed_recorded_clip_source(&clip_id, sample_rate, duration_seconds);
    Some(track_id)
}

impl StudioLayout {
    /// Tracks a track command acts on: the right-clicked track with the rest
    /// of the selection when it is part of it, else the selection.
    fn bounce_track_ids(&self, cx: &Context<Self>) -> Vec<String> {
        let selected = self.selected_track_ids(cx);
        match self.context_track_id_or_selected(cx) {
            Some(id) if !selected.contains(&id) => vec![id],
            _ => selected,
        }
    }

    pub(super) fn freeze_selected_tracks(&mut self, cx: &mut Context<Self>) {
        let ids = self.bounce_track_ids(cx);
        self.freeze_tracks(ids, cx);
    }

    /// The track header's freeze toggle.
    pub(super) fn toggle_track_freeze(&mut self, track_id: &str, cx: &mut Context<Self>) {
        let frozen = self
            .timeline
            .read(cx)
            .state
            .find_track(track_id)
            .is_some_and(|t| t.is_frozen());
        if frozen {
            self.unfreeze_tracks(vec![track_id.to_string()], cx);
        } else {
            self.freeze_tracks(vec![track_id.to_string()], cx);
        }
    }

    fn freeze_tracks(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let state = &self.timeline.read(cx).state;
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                state
                    .find_track(id)
                    .is_some_and(|t| t.track_type.can_freeze() && !t.is_frozen())
            })
            .collect();
        if ids.is_empty() {
            self.show_bounce_error(
                BounceKind::Freeze,
                "Select an audio or instrument track that is not frozen yet.",
                cx,
            );
            return;
        }
        self.start_bounce(BounceKind::Freeze, ids, cx);
    }

    pub(super) fn render_selected_tracks_in_place(&mut self, cx: &mut Context<Self>) {
        let ids = self.bounce_track_ids(cx);
        self.start_bounce(BounceKind::RenderInPlace, ids, cx);
    }

    pub(super) fn render_arrangement_selection(&mut self, cx: &mut Context<Self>) {
        let ids = {
            let state = &self.timeline.read(cx).state;
            match &state.arrangement_range {
                Some(range) if !range.track_ids.is_empty() => range.track_ids.clone(),
                Some(_) => self.selected_track_ids(cx),
                None => Vec::new(),
            }
        };
        if ids.is_empty() {
            self.show_bounce_error(
                BounceKind::RenderSelection,
                "Select a time range on the tracks to render first.",
                cx,
            );
            return;
        }
        self.start_bounce(BounceKind::RenderSelection, ids, cx);
    }

    pub(super) fn consolidate_selected_clips(&mut self, cx: &mut Context<Self>) {
        let track_id = {
            let state = &self.timeline.read(cx).state;
            let ids = &state.selection.selected_clip_ids;
            let owners: Vec<_> = ids.iter().filter_map(|id| state.find_clip(id)).collect();
            let one_track = owners
                .first()
                .map(|(track, _)| track.id.clone())
                .filter(|first| owners.iter().all(|(track, _)| track.id == *first));
            let all_audio = owners
                .iter()
                .all(|(_, clip)| matches!(clip.clip_type, ClipType::Audio { .. }));
            one_track.filter(|_| owners.len() >= 2 && owners.len() == ids.len() && all_audio)
        };
        let Some(track_id) = track_id else {
            self.show_bounce_error(
                BounceKind::Consolidate,
                "Select two or more audio clips on one track.",
                cx,
            );
            return;
        };
        self.start_bounce(BounceKind::Consolidate, vec![track_id], cx);
    }

    fn start_bounce(&mut self, kind: BounceKind, track_ids: Vec<String>, cx: &mut Context<Self>) {
        if self.bounce_running {
            self.show_bounce_error(kind, "Another render is still running.", cx);
            return;
        }
        if self.is_recording_active(cx) {
            self.show_bounce_error(kind, "Stop recording first.", cx);
            return;
        }
        let Some(project_root) = self.project_session.folder_path.clone() else {
            self.show_bounce_error(kind, "Save the project to a folder first.", cx);
            return;
        };
        let layout = crate::paths::ProjectFolderLayout::from_root(project_root.clone());
        let dir = match kind {
            BounceKind::Freeze => layout.rendered_freeze,
            _ => layout.rendered_bounces,
        };
        if let Err(error) = std::fs::create_dir_all(&dir) {
            self.show_bounce_error(
                kind,
                &format!("Could not create {}: {error}", dir.display()),
                cx,
            );
            return;
        }

        // The render takes the live plug-ins away from playback for its
        // duration, so playback stops first.
        self.dispatch_transport_command(super::studio_state::TransportCommand::Stop, cx);
        self.refresh_bridge_plugin_states(super::plugin_ops::PluginStateCaptureFor::Export, cx);
        let state = self.timeline.read(cx).state.clone();
        let sample_rate = self.current_audio_sample_rate();
        let (pdc_enabled, latency_graph_version) = match self.audio_bridge.engine.as_ref() {
            Some(engine) => (engine.pdc_enabled(), engine.latency_graph_version()),
            None => (
                self.settings.read(cx).current.playback.latency_compensation,
                0,
            ),
        };
        let bridge_sinks = self
            .audio_bridge
            .engine
            .as_ref()
            .map(|engine| engine.plugin_bridge_sinks())
            .unwrap_or_default();
        let root_string = project_root.to_string_lossy().into_owned();
        let mut snapshot = build_engine_project_snapshot_for_export(
            &state,
            sample_rate,
            Some(&root_string),
            None,
            pdc_enabled,
            latency_graph_version,
        );
        snapshot.routing.buffer_size = self
            .audio_bridge
            .engine
            .as_ref()
            .map(|engine| engine.stats().buffer_size)
            .filter(|size| *size > 0)
            .unwrap_or(snapshot.routing.buffer_size.max(1));

        let targets = match bounce_targets(kind, &state, &snapshot, &track_ids, &dir) {
            Ok(targets) => targets,
            Err(message) => {
                self.show_bounce_error(kind, &message, cx);
                return;
            }
        };
        isolate_for_bounce(&mut snapshot, &targets);

        let cancel = ExportCancelToken::new();
        let shared = Arc::new(Mutex::new(BounceShared::default()));
        let names: Vec<&str> = targets.iter().map(|t| t.track_name.as_str()).collect();
        let options = ProgressDialogOptions::default()
            .title(kind.title())
            .heading(format!("{}…", kind.title()))
            .detail(names.join(", "))
            .progress(ProgressBarValue::value(0.0))
            .cancel_label("Cancel");
        let on_cancel = {
            let cancel = cancel.clone();
            Arc::new(move |_: &mut gpui::Window, _: &mut gpui::App| cancel.cancel())
                as crate::components::progress_dialog::ProgressDialogCancelCb
        };
        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            None,
            self.studio_window_bounds(cx),
            cx,
        );
        let dialog = open_progress_dialog_window(owner_bounds, options, Some(on_cancel), cx).ok();
        self.bounce_running = true;

        let engine = self.audio_bridge.engine.clone();
        let worker_shared = shared.clone();
        let worker_cancel = cancel.clone();
        let tail = kind.tail();
        let spawned = std::thread::Builder::new()
            .name("fb-bounce".to_string())
            .spawn(move || {
                let result = {
                    let _handoff =
                        crate::export::BridgeSinkHandoff::detach(engine.as_ref(), &bridge_sinks);
                    render_targets(
                        &snapshot,
                        &targets,
                        sample_rate,
                        tail,
                        &worker_cancel,
                        &bridge_sinks,
                        &worker_shared,
                    )
                };
                if let Ok(mut guard) = worker_shared.lock() {
                    guard.done = Some(result);
                }
            });
        if let Err(error) = spawned {
            self.bounce_running = false;
            close_dialog(dialog.as_ref(), cx);
            self.show_bounce_error(kind, &format!("Could not start the render: {error}"), cx);
            return;
        }

        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let (progress, done) = match shared.lock() {
                Ok(mut guard) => (guard.progress, guard.done.take()),
                Err(_) => (
                    0.0,
                    Some(Err("the render stopped unexpectedly".to_string())),
                ),
            };
            if let Some(dialog) = dialog.as_ref() {
                let _ = dialog.update(cx, |dialog, _, cx| {
                    dialog.set_progress(ProgressBarValue::value(progress), cx);
                });
            }
            if let Some(result) = done {
                let _ = this.update(cx, |layout, cx| {
                    layout.bounce_running = false;
                    close_dialog(dialog.as_ref(), cx);
                    match result {
                        Ok(results) => layout.apply_bounce(kind, results, cx),
                        Err(_) if cancel.is_cancelled() => {}
                        Err(message) => layout.show_bounce_error(kind, &message, cx),
                    }
                });
                break;
            }
        })
        .detach();
    }

    /// The bounce's result in the project, as one undo step.
    fn apply_bounce(
        &mut self,
        kind: BounceKind,
        results: Vec<BounceResult>,
        cx: &mut Context<Self>,
    ) {
        if results.is_empty() {
            return;
        }
        let source_ids: Vec<String> = results.iter().map(|r| r.target.track_id.clone()).collect();
        let mut new_media: Vec<PathBuf> = Vec::new();
        let mut muted_sources: Vec<String> = Vec::new();
        self.timeline.update(cx, |timeline, cx| {
            let edit = timeline.begin_track_edit(TrackEditScope::tracks(source_ids.clone()));
            let state = &mut timeline.state;
            match kind {
                BounceKind::Freeze => {
                    for result in &results {
                        let Some(track) = state
                            .tracks
                            .iter_mut()
                            .find(|t| t.id == result.target.track_id)
                        else {
                            continue;
                        };
                        track.freeze = Some(TrackFreeze {
                            media_path: result.target.output.to_string_lossy().into_owned(),
                            start_beat: result.target.start_beat,
                            duration_seconds: result.duration_seconds,
                        });
                    }
                    let frozen_clips: Vec<String> = state
                        .tracks
                        .iter()
                        .filter(|t| t.is_frozen())
                        .flat_map(|t| t.clips.iter().map(|c| c.id.clone()))
                        .collect();
                    state
                        .selection
                        .selected_clip_ids
                        .retain(|id| !frozen_clips.contains(id));
                }
                BounceKind::RenderInPlace | BounceKind::RenderSelection => {
                    let mut last = None;
                    for result in &results {
                        let name = format!("{} (Rendered)", result.target.track_name);
                        last = add_bounced_track(
                            state,
                            &result.target.track_id,
                            name,
                            &result.target.output,
                            result.target.start_beat,
                            result.duration_seconds,
                            result.sample_rate,
                            false,
                        );
                        new_media.push(result.target.output.clone());
                        // A range leaves the rest of the source to play.
                        if kind == BounceKind::RenderInPlace
                            && state.set_track_mute(&result.target.track_id, true)
                        {
                            muted_sources.push(result.target.track_id.clone());
                        }
                    }
                    if let Some(last) = last {
                        state.select_track(&last);
                    }
                }
                BounceKind::Consolidate => {
                    let result = &results[0];
                    for id in result.target.clip_ids.iter().flatten() {
                        state.delete_clip(id);
                    }
                    let clip_id = state.insert_recorded_clip(
                        &result.target.track_id,
                        result.target.output.to_string_lossy().into_owned(),
                        format!("{} Consolidated", result.target.track_name),
                        result.target.start_beat as f32,
                        result.duration_seconds,
                        state.bpm,
                    );
                    state.seed_recorded_clip_source(
                        &clip_id,
                        result.sample_rate,
                        result.duration_seconds,
                    );
                    state.select_clip(&clip_id);
                    new_media.push(result.target.output.clone());
                }
            }
            timeline.commit_track_edit(kind.title(), edit, false, cx);
            cx.notify();
        });

        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            for id in &muted_sources {
                let _ = engine.update_track_param(id, "muted", 1.0);
            }
        }
        let timeline = self.timeline.clone();
        for path in new_media {
            let key = path.to_string_lossy().into_owned();
            self.spawn_timeline_audio_import_jobs(cx, timeline.clone(), path, key);
        }
        self.mark_dirty();
        self.mark_engine_media_dirty();
        self.schedule_audio_project_sync(cx, true, "bounce_apply");
        self.push_mixer_snapshot_to_window(cx);
        cx.notify();
    }

    pub(super) fn unfreeze_selected_tracks(&mut self, cx: &mut Context<Self>) {
        let ids = self.bounce_track_ids(cx);
        self.unfreeze_tracks(ids, cx);
    }

    fn unfreeze_tracks(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let ids: Vec<String> = {
            let state = &self.timeline.read(cx).state;
            ids.into_iter()
                .filter(|id| state.find_track(id).is_some_and(|t| t.is_frozen()))
                .collect()
        };
        if ids.is_empty() {
            return;
        }
        self.timeline.update(cx, |timeline, cx| {
            let edit = timeline.begin_track_edit(TrackEditScope::tracks(ids.clone()));
            for track in timeline
                .state
                .tracks
                .iter_mut()
                .filter(|t| ids.contains(&t.id))
            {
                track.freeze = None;
            }
            timeline.commit_track_edit("Unfreeze Track", edit, false, cx);
            cx.notify();
        });
        self.mark_dirty();
        self.mark_engine_media_dirty();
        self.schedule_audio_project_sync(cx, true, "unfreeze");
        self.push_mixer_snapshot_to_window(cx);
        cx.notify();
    }

    /// Replace each selected frozen track with an audio track playing its
    /// freeze, keeping the channel: its instrument, inserts and clips go.
    pub(super) fn flatten_selected_tracks(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = {
            let ids = self.bounce_track_ids(cx);
            let state = &self.timeline.read(cx).state;
            ids.into_iter()
                .filter(|id| state.find_track(id).is_some_and(|t| t.is_frozen()))
                .collect()
        };
        if ids.is_empty() {
            self.show_bounce_error(
                BounceKind::Freeze,
                "Only a frozen track can be flattened. Freeze it first.",
                cx,
            );
            return;
        }
        // Editors closed and live plug-in state kept before the undo record is
        // taken, so undo brings the plug-ins back as they sound now.
        for id in &ids {
            self.cleanup_track_plugins_before_delete(id, cx);
        }
        let sample_rate = self.current_audio_sample_rate();
        let mut new_media = Vec::new();
        self.timeline.update(cx, |timeline, cx| {
            let edit = timeline.begin_track_edit(TrackEditScope::tracks(ids.clone()));
            let state = &mut timeline.state;
            let mut last = None;
            for id in &ids {
                let Some(source) = state.find_track(id).cloned() else {
                    continue;
                };
                let Some(freeze) = source.freeze.clone() else {
                    continue;
                };
                let path = PathBuf::from(&freeze.media_path);
                last = add_bounced_track(
                    state,
                    id,
                    source.name.clone(),
                    &path,
                    freeze.start_beat,
                    freeze.duration_seconds,
                    sample_rate,
                    true,
                );
                state.delete_track(id);
                new_media.push(path);
            }
            if let Some(last) = last {
                state.select_track(&last);
            }
            timeline.commit_track_edit("Flatten Track", edit, false, cx);
            cx.notify();
        });
        let timeline = self.timeline.clone();
        for path in new_media {
            let key = path.to_string_lossy().into_owned();
            self.spawn_timeline_audio_import_jobs(cx, timeline.clone(), path, key);
        }
        self.mark_dirty();
        self.mark_engine_media_dirty();
        self.schedule_audio_project_sync(cx, true, "flatten");
        self.push_mixer_snapshot_to_window(cx);
        cx.notify();
    }

    /// Say why a bounce could not run, in the same dialog a running one uses.
    fn show_bounce_error(&mut self, kind: BounceKind, message: &str, cx: &mut Context<Self>) {
        eprintln!("[bounce] {}: {message}", kind.title());
        let options = ProgressDialogOptions::default()
            .title(kind.title())
            .heading(format!("{} could not run", kind.title()))
            .detail(message)
            .progress(ProgressBarValue::value(0.0))
            .hide_percent()
            .cancel_label("Close");
        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            None,
            self.studio_window_bounds(cx),
            cx,
        );
        let _ = open_progress_dialog_window(owner_bounds, options, None, cx);
    }
}

fn close_dialog(dialog: Option<&WindowHandle<ProgressDialogWindow>>, cx: &mut gpui::App) {
    if let Some(dialog) = dialog {
        let _ = dialog.update(cx, |_, window, _| window.remove_window());
    }
}

/// What `kind` renders for `track_ids`, or why it cannot.
fn bounce_targets(
    kind: BounceKind,
    state: &TimelineState,
    snapshot: &EngineProjectSnapshot,
    track_ids: &[String],
    dir: &Path,
) -> Result<Vec<BounceTarget>, String> {
    let mut targets = Vec::new();
    for id in track_ids {
        let Some(track) = state.find_track(id) else {
            continue;
        };
        if !track.track_type.can_freeze() {
            continue;
        }
        let (start_beat, end_beat, clip_ids) = match kind {
            BounceKind::Freeze | BounceKind::RenderInPlace => {
                let Some((start, end)) = track_content_range(snapshot, id) else {
                    continue;
                };
                (start, end, None)
            }
            BounceKind::RenderSelection => {
                let Some(range) = state.arrangement_range.as_ref() else {
                    continue;
                };
                (range.start_beat, range.end_beat, None)
            }
            BounceKind::Consolidate => {
                let selected = &state.selection.selected_clip_ids;
                let clips: Vec<_> = track
                    .clips
                    .iter()
                    .filter(|clip| selected.contains(&clip.id))
                    .collect();
                let start = clips
                    .iter()
                    .map(|c| c.start_beat as f64)
                    .fold(f64::INFINITY, f64::min);
                let end = clips
                    .iter()
                    .map(|c| (c.start_beat + c.duration_beats) as f64)
                    .fold(0.0, f64::max);
                (
                    start,
                    end,
                    Some(clips.iter().map(|c| c.id.clone()).collect()),
                )
            }
        };
        if !(end_beat > start_beat) {
            continue;
        }
        targets.push(BounceTarget {
            track_id: id.clone(),
            track_name: track.name.clone(),
            start_beat: start_beat.max(0.0),
            end_beat,
            clip_ids,
            output: unique_bounce_path(dir, &track.name, kind.file_suffix()),
        });
    }
    if targets.is_empty() {
        return Err(match kind {
            BounceKind::RenderSelection => {
                "The selected range covers no audio or instrument track.".to_string()
            }
            _ => "The selected tracks have nothing to render: only audio and instrument tracks \
                  with clips can be."
                .to_string(),
        });
    }
    Ok(targets)
}

/// Render each target to its file, one after another. Runs on the worker.
fn render_targets(
    snapshot: &EngineProjectSnapshot,
    targets: &[BounceTarget],
    sample_rate: u32,
    tail: ExportTailMode,
    cancel: &ExportCancelToken,
    bridge_sinks: &DirectAudio::plugin_bridge::PluginBridgeSinkMap,
    shared: &Arc<Mutex<BounceShared>>,
) -> Result<Vec<BounceResult>, String> {
    let count = targets.len().max(1) as f32;
    let mut results = Vec::with_capacity(targets.len());
    for (index, target) in targets.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        let job = RenderJob {
            mixdown: None,
            tracks: vec![TrackExportTarget {
                track_id: target.track_id.clone(),
                request: bounce_request(snapshot, target, sample_rate, tail),
            }],
        };
        let progress_shared = shared.clone();
        let on_progress = move |progress: DirectAudio::ExportProgress| {
            if let Ok(mut guard) = progress_shared.lock() {
                guard.progress = (index as f32 + progress.percent / 100.0) / count;
            }
        };
        let summaries =
            export_render_job_with_bridges(snapshot, &job, cancel, Some(bridge_sinks), on_progress)
                .map_err(|error| format!("{}: {error}", target.track_name))?;
        let summary = summaries
            .into_iter()
            .next()
            .ok_or_else(|| format!("{}: nothing was written", target.track_name))?;
        results.push(BounceResult {
            target: target.clone(),
            duration_seconds: summary.duration_seconds,
            sample_rate: summary.sample_rate,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use DirectAudio::types::{
        EngineAutomationLaneSnapshot, EngineAutomationTargetSnapshot, EngineClipSnapshot,
        EngineInsertSnapshot, EngineRoutingSnapshot, EngineTrackSnapshot,
    };

    fn track(id: &str) -> EngineTrackSnapshot {
        EngineTrackSnapshot {
            midi_programs: Vec::new(),
            id: id.to_string(),
            track_type: "audio".to_string(),
            volume: 0.5,
            pan: -0.5,
            muted: true,
            solo: true,
            armed: true,
            input_monitor: true,
            input_source: Default::default(),
            preview_mode: "stereo".to_string(),
            output_track_id: None,
            sends: Vec::new(),
            builtin_soundfont_player: false,
            soundfont_path: None,
            soundfont_preset_bank: None,
            soundfont_preset_patch: None,
            soundfont_volume: 1.0,
            soundfont_reverb_chorus: true,
            soundfont_polyphony: 64,
            soundfont_envelope: Default::default(),
            soundfont_quality: Default::default(),
            soundfont_mode: Default::default(),
            soundfont_channels: Default::default(),
            solfege_engine: None,
            inserts: vec![EngineInsertSnapshot {
                id: format!("{id}-fx"),
                kind: "fx".to_string(),
                enabled: true,
                params: Default::default(),
                state: None,
            }],
            automation_lanes: [0u8, 3]
                .into_iter()
                .map(|tag| EngineAutomationLaneSnapshot {
                    id: format!("{id}-lane-{tag}"),
                    name: String::new(),
                    target: EngineAutomationTargetSnapshot {
                        tag,
                        ..Default::default()
                    },
                    enabled: true,
                    points: Vec::new(),
                })
                .collect(),
        }
    }

    fn clip(id: &str, track_id: &str, start: f64, length: f64) -> EngineClipSnapshot {
        EngineClipSnapshot {
            id: id.to_string(),
            track_id: track_id.to_string(),
            asset_id: id.to_string(),
            media_path: Some(format!("{id}.wav")),
            start_beat: start,
            duration_beats: length,
            offset_seconds: 0.0,
            gain: 1.0,
            muted: false,
            ara_rendered: false,
            fades: None,
            stretch: Default::default(),
            audio_process: None,
        }
    }

    fn snapshot() -> EngineProjectSnapshot {
        EngineProjectSnapshot {
            spatial: Default::default(),
            project_id: "p".to_string(),
            project_root: None,
            preferred_input_device: None,
            bpm: 120.0,
            tempo_points: Vec::new(),
            time_signature: [4, 4],
            sample_rate: 48_000,
            tracks: vec![track("a"), track("b")],
            clips: vec![
                clip("a1", "a", 4.0, 2.0),
                clip("a2", "a", 10.0, 4.0),
                clip("b1", "b", 0.0, 8.0),
            ],
            midi_clips: Vec::new(),
            pdc_enabled: true,
            latency_graph_version: 1,
            routing: EngineRoutingSnapshot {
                master_output_device: None,
                sample_rate: 48_000,
                buffer_size: 256,
            },
        }
    }

    fn target(id: &str, clip_ids: Option<Vec<String>>) -> BounceTarget {
        BounceTarget {
            track_id: id.to_string(),
            track_name: id.to_string(),
            start_beat: 0.0,
            end_beat: 1.0,
            clip_ids,
            output: PathBuf::from("out.wav"),
        }
    }

    #[test]
    fn content_range_spans_the_tracks_clips() {
        assert_eq!(track_content_range(&snapshot(), "a"), Some((4.0, 14.0)));
        assert_eq!(track_content_range(&snapshot(), "none"), None);
    }

    #[test]
    fn isolation_keeps_only_the_target_at_unity_before_its_fader() {
        let mut snap = snapshot();
        isolate_for_bounce(&mut snap, &[target("a", None)]);
        assert!(snap.clips.iter().all(|c| c.track_id == "a"));
        assert_eq!(snap.clips.len(), 2);
        let a = snap.tracks.iter().find(|t| t.id == "a").unwrap();
        assert_eq!((a.volume, a.pan, a.muted, a.solo), (1.0, 0.0, false, false));
        assert!(a.inserts[0].enabled, "a track render keeps its inserts");
        let tags: Vec<u8> = a.automation_lanes.iter().map(|l| l.target.tag).collect();
        assert_eq!(
            tags,
            vec![3],
            "fader automation goes, plug-in automation stays"
        );
        let b = snap.tracks.iter().find(|t| t.id == "b").unwrap();
        assert!(!b.solo && !b.armed && !b.input_monitor);
    }

    #[test]
    fn consolidate_isolation_keeps_the_chosen_clips_without_inserts() {
        let mut snap = snapshot();
        isolate_for_bounce(&mut snap, &[target("a", Some(vec!["a2".to_string()]))]);
        let ids: Vec<&str> = snap.clips.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["a2"]);
        let a = snap.tracks.iter().find(|t| t.id == "a").unwrap();
        assert!(!a.inserts[0].enabled);
    }
}

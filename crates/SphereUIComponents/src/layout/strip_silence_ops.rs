//! Audio › Strip Silence on the selected audio clips.
//!
//! The command opens the settings window for the clips it acts on: the
//! selected audio clips, or the clip a context menu was opened on when that
//! clip is not selected. Apply reads each clip's audio — the part of its file
//! it plays, in the order it plays it — off the UI thread as a cancellable
//! Background Task, finds where it is above the threshold
//! (`detect_kept_regions`), and replaces each clip by the pieces that keep
//! that audio (`TimelineState::plan_strip_silence`). Every clip changes in
//! one undo step. A clip with nothing above the threshold is left as it is
//! and named in the task's result; a cancelled or failed job changes
//! nothing.

use std::sync::Arc;

use gpui::{AppContext, Context};

use crate::components::background_tasks::{BackgroundJob, BackgroundTaskKind};
use crate::components::edit::{ClipSnapshot, EditCommand};
use crate::components::strip_silence_dialog::open_strip_silence_window;
use crate::components::timeline::timeline_state::{
    detect_kept_regions, next_clip_id_number, AudioClipConversionSource, StripSilenceOutcome,
    StripSilenceParams, TrackEditScope,
};

use super::StudioLayout;

/// Kept regions per clip, in seconds into its played window, or why the
/// clip could not be read.
type Analysis = Vec<(AudioClipConversionSource, Result<Vec<(f64, f64)>, String>)>;

/// Reads every clip's audio and finds what to keep. Background thread only.
/// `None` when the job was cancelled.
fn analyse(
    job: &BackgroundJob,
    sources: Vec<AudioClipConversionSource>,
    params: &StripSilenceParams,
) -> Option<Analysis> {
    let total = sources.len() as u32;
    let mut out = Vec::with_capacity(sources.len());
    // Pieces of one recording share a file: decode it once.
    let mut decoded: Option<(String, Result<_, String>)> = None;
    for (index, source) in sources.into_iter().enumerate() {
        if job.is_cancelled() {
            return None;
        }
        job.set_detail(source.clip_name.clone());
        job.set_progress(index as u32, total);
        if decoded
            .as_ref()
            .is_none_or(|(path, _)| *path != source.path)
        {
            let buffer = DirectAudio::load_audio_file_for_edit(&source.path)
                .map_err(|error| format!("Could not read the clip's audio: {error}"));
            decoded = Some((source.path.clone(), buffer));
        }
        let result = match decoded.as_ref().map(|(_, buffer)| buffer) {
            Some(Ok(buffer)) => {
                let channels = (buffer.channels as usize).max(1);
                let frames = buffer.samples.len() / channels;
                let range = source.frames_in(frames, buffer.sample_rate);
                // Each frame's RMS across its channels: out-of-phase
                // channels must not cancel into "silence".
                let mut level: Vec<f32> = range
                    .map(|frame| {
                        let at = frame * channels;
                        let power = buffer.samples[at..at + channels]
                            .iter()
                            .map(|s| s * s)
                            .sum::<f32>()
                            / channels as f32;
                        power.sqrt()
                    })
                    .collect();
                if source.reverse {
                    level.reverse();
                }
                if level.is_empty() {
                    Err("The clip plays no audio.".to_string())
                } else {
                    Ok(detect_kept_regions(&level, buffer.sample_rate, params))
                }
            }
            Some(Err(message)) => Err(message.clone()),
            None => Err("Could not read the clip's audio.".to_string()),
        };
        out.push((source, result));
    }
    job.set_progress(total, total);
    Some(out)
}

/// What Strip Silence did, for the Background Tasks result.
#[derive(Debug, Default, PartialEq)]
struct StripReport {
    /// Pieces kept across every clip that was cut.
    kept: usize,
    /// Clips that were cut.
    cut: usize,
    /// Clips with nothing above the threshold, left as they were.
    silent: Vec<String>,
    /// Clips whose audio is above the threshold throughout.
    unchanged: usize,
    /// Clips that were edited, moved away or deleted while being analysed.
    changed: usize,
    /// Clips that could not be read, with why.
    failed: Vec<String>,
}

impl StripReport {
    fn summary(&self) -> String {
        let plural = |n: usize, one: &str, many: &str| {
            if n == 1 {
                format!("1 {one}")
            } else {
                format!("{n} {many}")
            }
        };
        let mut parts = Vec::new();
        if self.cut > 0 {
            parts.push(format!(
                "{} kept from {}",
                plural(self.kept, "region", "regions"),
                plural(self.cut, "clip", "clips")
            ));
        }
        match self.silent.as_slice() {
            [] => {}
            [one] => parts.push(format!("{one} is silent at this threshold (left as is)")),
            many => parts.push(format!(
                "{} silent at this threshold (left as is)",
                plural(many.len(), "clip", "clips")
            )),
        }
        if self.unchanged > 0 {
            parts.push(format!(
                "{} with no silence to strip",
                plural(self.unchanged, "clip", "clips")
            ));
        }
        if self.changed > 0 {
            parts.push(format!(
                "{} changed while analysed (skipped)",
                plural(self.changed, "clip", "clips")
            ));
        }
        if !self.failed.is_empty() {
            parts.push(format!(
                "{} could not be read",
                plural(self.failed.len(), "clip", "clips")
            ));
        }
        if parts.is_empty() {
            "Nothing to strip".to_string()
        } else {
            parts.join("; ")
        }
    }
}

impl StudioLayout {
    /// The clips Strip Silence acts on: the clip a context menu is open on
    /// when it is not selected, else every selected audio clip whose audio
    /// is ready.
    fn strip_silence_targets(&self, cx: &gpui::App) -> Vec<String> {
        let state = &self.timeline.read(cx).state;
        let ready = |clip_id: &String| state.audio_clip_conversion_source(clip_id).is_ok();
        let selected = &state.selection.selected_clip_ids;
        match self.context_menu_clip_id() {
            Some(clip_id) if !selected.contains(&clip_id) => {
                if ready(&clip_id) {
                    vec![clip_id]
                } else {
                    Vec::new()
                }
            }
            _ => selected.iter().filter(|id| ready(id)).cloned().collect(),
        }
    }

    /// Audio › Strip Silence as the menu bar shows it: available while a
    /// selected clip is an audio clip that can be read.
    pub(super) fn strip_silence_menu_states(
        &self,
        cx: &gpui::App,
    ) -> Vec<crate::menu::CommandMenuState<'static>> {
        vec![crate::menu::CommandMenuState {
            command: "audio:strip-silence",
            enabled: !self.strip_silence_targets(cx).is_empty(),
            detail: None,
        }]
    }

    /// Opens the Strip Silence window for the clips the command acts on.
    pub(super) fn open_strip_silence(&mut self, cx: &mut Context<Self>) {
        let clip_ids = self.strip_silence_targets(cx);
        if clip_ids.is_empty() {
            return;
        }
        let target = match clip_ids.len() {
            1 => {
                let state = &self.timeline.read(cx).state;
                state
                    .find_clip(&clip_ids[0])
                    .map(|(_, clip)| clip.name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "1 clip".to_string())
            }
            n => format!("{n} clips"),
        };
        let owner_bounds = self.studio_window_bounds(cx);
        let layout = cx.entity();
        let on_apply = Arc::new(move |params: StripSilenceParams, cx: &mut gpui::App| {
            let clip_ids = clip_ids.clone();
            StudioLayout::defer_update(&layout, cx, move |this, cx| {
                this.strip_silence(clip_ids, params, cx);
            });
        });
        // Deferred: the new window draws once before `open_window` returns,
        // and this layout is leased for the length of this update.
        cx.defer(move |cx| {
            if let Err(error) = open_strip_silence_window(owner_bounds, target, on_apply, cx) {
                eprintln!("[strip-silence] could not open the window: {error}");
            }
        });
    }

    /// Analyses `clip_ids` off the UI thread and strips their silence.
    fn strip_silence(
        &mut self,
        clip_ids: Vec<String>,
        params: StripSilenceParams,
        cx: &mut Context<Self>,
    ) {
        let (sources, generation) = {
            let timeline = self.timeline.read(cx);
            let sources: Vec<_> = clip_ids
                .iter()
                .filter_map(|clip_id| timeline.state.audio_clip_conversion_source(clip_id).ok())
                .collect();
            (sources, timeline.project_generation())
        };
        if sources.is_empty() {
            BackgroundJob::start(BackgroundTaskKind::Analysis, "Strip Silence")
                .fail("No selected audio clip can be read.");
            return;
        }
        let title = match sources.as_slice() {
            [one] => format!("Strip Silence: {}", one.clip_name),
            many => format!("Strip Silence: {} clips", many.len()),
        };
        let job = BackgroundJob::start_cancellable(BackgroundTaskKind::Analysis, title);
        cx.spawn(async move |this, cx| {
            let (job, analysis) = cx
                .background_spawn(async move {
                    let analysis = analyse(&job, sources, &params);
                    (job, analysis)
                })
                .await;
            let _ = this.update(cx, |layout, cx| {
                layout.finish_strip_silence(job, generation, analysis, params, cx);
            });
        })
        .detach();
    }

    fn finish_strip_silence(
        &mut self,
        job: BackgroundJob,
        generation: u64,
        analysis: Option<Analysis>,
        params: StripSilenceParams,
        cx: &mut Context<Self>,
    ) {
        let Some(analysis) = analysis.filter(|_| !job.is_cancelled()) else {
            // Dropped without an outcome: recorded as stopped.
            return;
        };
        if self.timeline.read(cx).project_generation() != generation {
            job.fail("The project was closed while the clips were being analysed.");
            return;
        }
        let report = self.timeline.update(cx, |timeline, cx| {
            let mut report = StripReport::default();
            let mut replacements: Vec<EditCommand> = Vec::new();
            let mut tracks: Vec<String> = Vec::new();
            let mut pieces: Vec<String> = Vec::new();
            {
                let state = &timeline.state;
                let mut next_id = next_clip_id_number(&state.tracks);
                for (source, result) in analysis {
                    let regions = match result {
                        Ok(regions) => regions,
                        Err(message) => {
                            report
                                .failed
                                .push(format!("{}: {message}", source.clip_name));
                            continue;
                        }
                    };
                    // Cut only what was analysed: the clip as it was read.
                    if state.audio_clip_conversion_source(&source.clip_id).as_ref() != Ok(&source) {
                        report.changed += 1;
                        continue;
                    }
                    match state.plan_strip_silence(
                        &source.clip_id,
                        &regions,
                        params.fade_ms,
                        &mut next_id,
                    ) {
                        StripSilenceOutcome::Pieces(kept) => {
                            let Some(snapshot) = ClipSnapshot::capture(state, &source.clip_id)
                            else {
                                report.changed += 1;
                                continue;
                            };
                            let track_id = snapshot.track_id.clone();
                            if !tracks.contains(&track_id) {
                                tracks.push(track_id.clone());
                            }
                            report.cut += 1;
                            report.kept += kept.len();
                            pieces.extend(kept.iter().map(|clip| clip.id.clone()));
                            replacements.push(EditCommand::ReplaceClipWithClips {
                                clips: kept
                                    .into_iter()
                                    .map(|clip| (track_id.clone(), clip))
                                    .collect(),
                                snapshot,
                            });
                        }
                        StripSilenceOutcome::Silent => report.silent.push(source.clip_name),
                        StripSilenceOutcome::Unchanged => report.unchanged += 1,
                        StripSilenceOutcome::Missing => report.changed += 1,
                    }
                }
            }
            if replacements.is_empty() {
                return report;
            }
            // Where each piece came from, now that no more planning splits
            // will note scratch ids: an ARA plug-in's edits follow the clip
            // into its pieces.
            for replacement in &replacements {
                if let EditCommand::ReplaceClipWithClips { snapshot, clips } = replacement {
                    for (_, piece) in clips {
                        timeline
                            .state
                            .clip_lineage
                            .record(&piece.id, &snapshot.clip.id);
                    }
                }
            }
            // Every clip's pieces (and takes) come back whole in one undo.
            let edit = timeline.begin_track_edit(TrackEditScope::tracks(tracks));
            for replacement in &replacements {
                replacement.execute(&mut timeline.state);
            }
            timeline.state.selection.selected_clip_ids = pieces;
            timeline.commit_track_edit("Strip Silence", edit, false, cx);
            timeline.mark_project_changed(cx);
            timeline.mark_media_changed(cx);
            cx.notify();
            report
        });
        if report.cut > 0 {
            self.mark_dirty();
            self.mark_engine_media_dirty();
            self.schedule_audio_project_sync(cx, false, "strip_silence");
            cx.notify();
        }
        if report.cut == 0 && !report.failed.is_empty() && report.silent.is_empty() {
            job.fail(report.failed.join("\n"));
        } else {
            job.complete(Some(report.summary()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_result_says_what_was_kept_and_what_was_left() {
        let report = StripReport {
            kept: 12,
            cut: 3,
            ..StripReport::default()
        };
        assert_eq!(report.summary(), "12 regions kept from 3 clips");

        let report = StripReport {
            kept: 1,
            cut: 1,
            silent: vec!["Room".into()],
            ..StripReport::default()
        };
        assert_eq!(
            report.summary(),
            "1 region kept from 1 clip; Room is silent at this threshold (left as is)"
        );

        let report = StripReport {
            silent: vec!["A".into(), "B".into()],
            unchanged: 1,
            ..StripReport::default()
        };
        assert_eq!(
            report.summary(),
            "2 clips silent at this threshold (left as is); 1 clip with no silence to strip"
        );
        assert_eq!(StripReport::default().summary(), "Nothing to strip");
    }
}

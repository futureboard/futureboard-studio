//! Async clip tempo analysis for the Audio Stretch inspector.
//!
//! Runs on the GPUI background executor — never blocks the UI thread and never
//! performs DSP rendering. Results write back to `AudioClipStretchState::bpm_source`.

use std::collections::HashMap;
use std::path::Path;

use gpui::Context;
use DirectAudio::{open_clip_audio_source, read_frame_stereo};

use crate::components::panel::StretchTempoUiSnapshot;
use crate::components::timeline::timeline_state::{
    detect_tempo_from_mono, tempo_picker_alternatives, AudioClipStretchState, TempoCandidate,
    TempoDetectionResult,
};

use super::StudioLayout;

const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;

#[derive(Debug, Clone, Default)]
pub(super) struct StretchTempoJob {
    pub finding: bool,
    pub error: Option<String>,
    pub alternatives: Vec<f32>,
    pub confidence: Option<f32>,
    pub low_confidence: bool,
    pub suggested_bpm: Option<f32>,
    pub pending_fit_project: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct StretchTempoState {
    jobs: HashMap<String, StretchTempoJob>,
}

#[derive(Debug, Clone)]
struct TempoAnalysisSuccess {
    detection: TempoDetectionResult,
    trim_start: u64,
    trim_end: u64,
    total_source_frames: u64,
    sample_rate: f32,
    channels: usize,
    decoded_samples: usize,
    analysis_seconds: f32,
}

impl StretchTempoState {
    pub fn snapshot_for_clip(&self, clip_id: &str) -> StretchTempoUiSnapshot {
        self.jobs.get(clip_id).map_or(
            StretchTempoUiSnapshot {
                finding: false,
                error: None,
                alternatives: Vec::new(),
                confidence: None,
                low_confidence: false,
                suggested_bpm: None,
            },
            |job| StretchTempoUiSnapshot {
                finding: job.finding,
                error: job.error.clone(),
                alternatives: job.alternatives.clone(),
                confidence: job.confidence,
                low_confidence: job.low_confidence,
                suggested_bpm: job.suggested_bpm,
            },
        )
    }

    fn job_mut(&mut self, clip_id: &str) -> &mut StretchTempoJob {
        self.jobs.entry(clip_id.to_string()).or_default()
    }

    pub fn begin_find(&mut self, clip_id: &str, pending_fit_project: bool) {
        let job = self.job_mut(clip_id);
        job.finding = true;
        job.error = None;
        job.alternatives.clear();
        job.confidence = None;
        job.low_confidence = false;
        job.suggested_bpm = None;
        job.pending_fit_project = pending_fit_project;
    }

    pub fn fail_find(&mut self, clip_id: &str, error: impl Into<String>) {
        let job = self.job_mut(clip_id);
        job.finding = false;
        job.error = Some(error.into());
        job.alternatives.clear();
        job.confidence = None;
        job.low_confidence = false;
        job.suggested_bpm = None;
        job.pending_fit_project = false;
    }

    pub fn complete_find_suggested(
        &mut self,
        clip_id: &str,
        detection: &TempoDetectionResult,
    ) -> bool {
        let job = self.job_mut(clip_id);
        let pending = job.pending_fit_project;
        job.finding = false;
        job.error = if detection.low_confidence {
            Some("Not sure of the tempo. Pick a candidate, or drag Source to set it.".to_string())
        } else {
            None
        };
        job.alternatives = tempo_picker_alternatives(detection);
        job.confidence = Some(detection.confidence);
        job.low_confidence = detection.low_confidence;
        job.suggested_bpm = Some(detection.bpm);
        job.pending_fit_project = false;
        pending
    }

    pub fn complete_find_applied(
        &mut self,
        clip_id: &str,
        detection: &TempoDetectionResult,
    ) -> bool {
        let job = self.job_mut(clip_id);
        let pending = job.pending_fit_project;
        job.finding = false;
        job.error = None;
        job.alternatives = tempo_picker_alternatives(detection);
        job.confidence = Some(detection.confidence);
        job.low_confidence = detection.low_confidence;
        job.suggested_bpm = None;
        job.pending_fit_project = false;
        pending
    }

    pub fn clear_error(&mut self, clip_id: &str) {
        if let Some(job) = self.jobs.get_mut(clip_id) {
            job.error = None;
        }
    }
}

/// Whether a detection result is confident enough to commit
/// `clip.stretch.source_bpm` automatically. Low-confidence results are surfaced
/// as candidates instead and must never silently overwrite the committed BPM or
/// drive Fit Project (spec Fix 1/8).
fn should_auto_commit(detection: &TempoDetectionResult) -> bool {
    !detection.low_confidence
}

fn tempo_find_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_STRETCH_TEMPO_DEBUG").is_some()
            || crate::components::panel::inspector_debug_enabled()
    })
}

fn log_tempo_find(message: impl AsRef<str>) {
    if tempo_find_debug_enabled() {
        eprintln!("[stretch-tempo] {}", message.as_ref());
    }
}

fn log_tempo_candidates(
    candidates: &[TempoCandidate],
    selected: &TempoDetectionResult,
    project_bpm: f64,
) {
    if !tempo_find_debug_enabled() {
        return;
    }
    eprintln!("[stretch-tempo] project_bpm={project_bpm:.2} candidates:");
    for (i, candidate) in candidates.iter().take(10).enumerate() {
        eprintln!(
            "  {}. {:.2} bpm score {:.3} relation {:?}",
            i + 1,
            candidate.bpm,
            candidate.confidence,
            candidate.relation
        );
    }
    eprintln!(
        "[stretch-tempo] alternatives: {}",
        selected
            .alternatives
            .iter()
            .map(|b| format!("{b:.2}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "[stretch-tempo] selected={:.2} bpm confidence={:.3} low_confidence={} confirm_required={} reason={}",
        selected.bpm,
        selected.confidence,
        selected.low_confidence,
        selected.low_confidence,
        selected.selection_reason
    );
}

fn extract_clip_trim_mono(
    source_path: &str,
    stretch: &AudioClipStretchState,
) -> Result<(Vec<f32>, TempoAnalysisSuccess), String> {
    let source = open_clip_audio_source(source_path)
        .map_err(|err| format!("Could not decode audio for tempo detection: {err}"))?;
    let sample_rate = source.sample_rate() as f32;
    let channels = source.channels();
    if sample_rate <= 0.0 {
        return Err("Could not decode audio for tempo detection: invalid sample rate".to_string());
    }

    let total_source_frames = source.frames() as u64;
    if total_source_frames == 0 {
        return Err("Audio source is still loading".to_string());
    }

    let (start, end) = stretch.resolved_source_trim_range(total_source_frames);
    let trim_len = end.saturating_sub(start);
    if trim_len == 0 {
        return Err("Clip source range is empty".to_string());
    }

    let trim_seconds = trim_len as f32 / sample_rate;
    let mut mono = Vec::with_capacity(trim_len as usize);
    for frame in start..end {
        let (l, r) = read_frame_stereo(&source, frame as usize);
        mono.push((l + r) * 0.5);
    }

    let decoded_samples = mono.len();
    let analysis_seconds = trim_seconds.min(60.0);
    log_tempo_find(format!(
        "decode: channels={channels} source_len_samples={total_source_frames} trim_start={start} trim_end={end} trim_duration_seconds={trim_seconds:.3} decoded_samples={decoded_samples} analysis_duration_seconds={analysis_seconds:.3} sample_rate={sample_rate}",
    ));

    Ok((
        mono,
        TempoAnalysisSuccess {
            detection: TempoDetectionResult {
                bpm: 0.0,
                confidence: 0.0,
                low_confidence: true,
                alternatives: Vec::new(),
                candidates: Vec::new(),
                selection_reason: String::new(),
            },
            trim_start: start,
            trim_end: end,
            total_source_frames,
            sample_rate,
            channels,
            decoded_samples,
            analysis_seconds,
        },
    ))
}

fn analyze_clip_tempo(
    source_path: &str,
    stretch: &AudioClipStretchState,
    project_bpm: f64,
) -> Result<TempoAnalysisSuccess, String> {
    let (mono, mut meta) = extract_clip_trim_mono(source_path, stretch)?;
    let detection = detect_tempo_from_mono(
        &mono,
        meta.sample_rate,
        MIN_BPM,
        MAX_BPM,
        Some(project_bpm as f32),
    )
    .ok_or_else(|| "Could not detect tempo".to_string())?;
    log_tempo_candidates(&detection.candidates, &detection, project_bpm);
    meta.detection = detection;
    Ok(meta)
}

impl StudioLayout {
    pub(super) fn stretch_tempo_snapshot(&self, clip_id: &str) -> StretchTempoUiSnapshot {
        self.stretch_tempo.snapshot_for_clip(clip_id)
    }

    pub(super) fn spawn_clip_tempo_detection(
        &mut self,
        clip_id: &str,
        pending_fit_project: bool,
        cx: &mut Context<Self>,
    ) {
        let resolved = {
            let timeline = self.timeline.read(cx);
            let Some((_, clip)) = timeline.state.find_clip(clip_id) else {
                self.stretch_tempo
                    .fail_find(clip_id, "No audio clip selected");
                cx.notify();
                return;
            };
            let source_path = match &clip.clip_type {
                crate::components::timeline::timeline_state::ClipType::Audio {
                    source_path,
                    ..
                } => source_path.clone(),
                _ => {
                    self.stretch_tempo
                        .fail_find(clip_id, "Selected item is not an audio clip");
                    cx.notify();
                    return;
                }
            };
            if source_path.is_none() {
                self.stretch_tempo
                    .fail_find(clip_id, "Selected clip has no audio source");
                cx.notify();
                return;
            }
            Some((
                source_path.unwrap(),
                clip.stretch.clone(),
                timeline.state.bpm as f64,
                clip.stretch.source_start_samples,
                clip.stretch.source_end_samples,
                clip.stretch.original_duration_samples,
                clip.stretch.source_len_samples(),
            ))
        };

        let Some((
            path,
            stretch,
            project_bpm,
            stored_start,
            stored_end,
            original_duration_samples,
            stored_len,
        )) = resolved
        else {
            return;
        };

        if !Path::new(&path).exists() {
            self.stretch_tempo
                .fail_find(clip_id, "Selected clip has no audio source");
            cx.notify();
            return;
        }

        log_tempo_find(format!(
            "Auto Find BPM: selected_clip_id={clip_id} source_path={path} source_len_samples={stored_len} trim_start_samples={stored_start} trim_end_samples={stored_end} original_duration_samples={original_duration_samples} project_bpm={project_bpm:.2}"
        ));

        self.stretch_tempo.begin_find(clip_id, pending_fit_project);
        cx.notify();

        let clip_id = clip_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { analyze_clip_tempo(&path, &stretch, project_bpm) })
                .await;

            let _ = this.update(cx, move |this, cx| {
                if this.timeline.read(cx).state.find_clip(&clip_id).is_none() {
                    this.stretch_tempo
                        .fail_find(&clip_id, "No audio clip selected");
                    cx.notify();
                    return;
                }

                match result {
                    Ok(success) => {
                        let detection = &success.detection;
                        // Low-confidence results are never auto-committed — not even
                        // when the user pressed Fit Project (which auto-finds first).
                        // They are surfaced as candidates for the user to pick/confirm
                        // so an unreliable BPM can't silently overwrite source_bpm or
                        // drive Fit Project (spec Fix 1/8).
                        if should_auto_commit(detection) {
                            let pending = this
                                .stretch_tempo
                                .complete_find_applied(&clip_id, detection);
                            this.apply_detected_source_bpm(&clip_id, success, pending, cx);
                        } else {
                            let _pending = this
                                .stretch_tempo
                                .complete_find_suggested(&clip_id, detection);
                        }
                    }
                    Err(error) => {
                        log_tempo_find(format!(
                            "Auto Find BPM failed: clip_id={clip_id} error={error}"
                        ));
                        this.stretch_tempo.fail_find(&clip_id, error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_detected_source_bpm(
        &mut self,
        clip_id: &str,
        success: TempoAnalysisSuccess,
        pending_fit_project: bool,
        cx: &mut Context<Self>,
    ) {
        let project_bpm = self.timeline.read(cx).state.bpm as f64;
        let bpm = success.detection.bpm as f64;
        let changed = self.timeline.update(cx, |timeline, cx| {
            let Some(prev) = timeline.state.clip_stretch(clip_id).cloned() else {
                return false;
            };
            let mut next = prev.clone();
            next.bpm_source = Some(bpm);
            next.clip_timeline_duration_beats = 0.0;
            next.dirty = true;

            if next.source_end_samples <= next.source_start_samples
                && success.trim_end > success.trim_start
            {
                next.source_start_samples = success.trim_start;
                next.source_end_samples = success.trim_end;
                next.original_duration_samples = next
                    .original_duration_samples
                    .max(success.total_source_frames);
                if next.project_sample_rate == 0 && success.sample_rate > 0.0 {
                    next.project_sample_rate = success.sample_rate.round() as u32;
                }
                if next.original_sample_rate == 0 && success.sample_rate > 0.0 {
                    next.original_sample_rate = success.sample_rate.round() as u32;
                }
            }

            if pending_fit_project {
                next = next.fitted_to_source_bpm(bpm, project_bpm);
            }
            if prev == next {
                return false;
            }
            let prev_len = timeline.state.clip_duration_beats(clip_id).unwrap_or(0.0);
            // The length the clip plays at, through the tempo map; the ratio
            // change only scales it while the source is still undecoded.
            let next_len = timeline
                .state
                .audio_clip_beats_with_stretch(clip_id, &next)
                .unwrap_or_else(|| {
                    let old_ratio = prev.effective_time_ratio(project_bpm);
                    let new_ratio = next.effective_time_ratio(project_bpm);
                    if old_ratio > 1e-6 && (old_ratio - new_ratio).abs() > 1e-9 {
                        (prev_len as f64 * (new_ratio / old_ratio)) as f32
                    } else {
                        prev_len
                    }
                });
            timeline.state.set_clip_stretch(clip_id, next.clone());
            if (next_len - prev_len).abs() > 1e-4 {
                timeline.state.set_clip_length(clip_id, next_len);
            }
            timeline.record_executed_command(
                crate::components::edit::EditCommand::SetClipStretch {
                    clip_id: clip_id.to_string(),
                    prev,
                    next,
                    prev_duration_beats: prev_len,
                    next_duration_beats: next_len,
                },
                cx,
            );
            true
        });
        if changed {
            self.mark_dirty();
            self.mark_engine_media_dirty();
            self.schedule_audio_project_sync(cx, false, "inspector_auto_find_bpm");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::AudioClipStretchState;

    #[test]
    fn stretch_tempo_state_tracks_finding_flag() {
        let mut state = StretchTempoState::default();
        state.begin_find("clip-1", false);
        let snap = state.snapshot_for_clip("clip-1");
        assert!(snap.finding);
        let detection = TempoDetectionResult {
            bpm: 120.0,
            confidence: 0.8,
            low_confidence: false,
            alternatives: vec![120.0, 60.0],
            candidates: Vec::new(),
            selection_reason: "test".to_string(),
        };
        state.complete_find_applied("clip-1", &detection);
        let snap = state.snapshot_for_clip("clip-1");
        assert!(!snap.finding);
        assert_eq!(snap.alternatives.len(), 2);
    }

    #[test]
    fn unresolved_trim_uses_full_source_length_in_analysis() {
        let stretch = AudioClipStretchState::default();
        assert_eq!(stretch.resolved_source_trim_range(96_000), (0, 96_000));
    }

    #[test]
    fn low_confidence_detection_is_not_auto_committed() {
        let low = TempoDetectionResult {
            bpm: 124.03,
            confidence: 0.0,
            low_confidence: true,
            alternatives: vec![118.0, 120.0, 124.0],
            candidates: Vec::new(),
            selection_reason: "test".to_string(),
        };
        assert!(
            !should_auto_commit(&low),
            "0% confidence must require user confirmation, not overwrite source_bpm"
        );

        let confident = TempoDetectionResult {
            confidence: 0.7,
            low_confidence: false,
            ..low
        };
        assert!(should_auto_commit(&confident));
    }
}

//! Convert to MIDI on an audio clip.
//!
//! The command exists only while an edition-provided converter is installed
//! (`crate::audio_to_midi`). Studio reads the clip's audio — the part of the
//! file it plays, mixed to mono and reversed when the clip is — hands it to
//! the converter off the UI thread, and writes what comes back as a MIDI clip
//! over the same span on a new MIDI track right under the clip's track. The
//! new track and its clip are one undo step. The job shows in Background
//! Tasks, where it can be cancelled; a cancelled or failed job changes
//! nothing.

use gpui::{AppContext, Context};

use crate::audio_to_midi::{audio_to_midi_converter, ConvertMode, ConvertedNote};
use crate::components::background_tasks::{BackgroundJob, BackgroundTaskKind};
use crate::components::edit::EditCommand;
use crate::components::timeline::timeline_state::{
    AudioClipConversionSource, ClipType, TrackEditScope,
};

use super::StudioLayout;

/// Reads the clip's audio and runs the converter. Background thread only.
fn convert(
    source: &AudioClipConversionSource,
    mode: ConvertMode,
    converter: &crate::audio_to_midi::AudioToMidiConverter,
) -> Result<Vec<ConvertedNote>, String> {
    let buffer = DirectAudio::load_audio_file_for_edit(&source.path)
        .map_err(|error| format!("Could not read the clip's audio: {error}"))?;
    let channels = (buffer.channels as usize).max(1);
    let total = buffer.samples.len() / channels;
    let range = source.frames_in(total, buffer.sample_rate);
    let mut mono: Vec<f32> = range
        .map(|frame| {
            let at = frame * channels;
            buffer.samples[at..at + channels].iter().sum::<f32>() / channels as f32
        })
        .collect();
    if source.reverse {
        mono.reverse();
    }
    if mono.is_empty() {
        return Err("The clip plays no audio.".into());
    }
    Ok(converter(&mono, buffer.sample_rate, mode))
}

/// The new track's name: the clip's, and what it was heard as.
fn track_name(clip_name: &str, mode: ConvertMode) -> String {
    let base = clip_name.trim();
    let base = if base.is_empty() { "Audio" } else { base };
    format!("{base} ({} MIDI)", mode.label())
}

impl StudioLayout {
    /// Whether this build and license offer Convert to MIDI.
    pub(super) fn audio_to_midi_available(&self) -> bool {
        audio_to_midi_converter().is_some()
    }

    /// The clip a conversion acts on: the clip a context menu is open on, or
    /// else (Audio › Audio to MIDI) the first selected clip that can be
    /// converted, so a selection mixing MIDI and audio clips still works.
    fn audio_to_midi_target(&self, cx: &gpui::App) -> Option<String> {
        if let Some(clip_id) = self.context_menu_clip_id() {
            return Some(clip_id);
        }
        let state = &self.timeline.read(cx).state;
        state
            .selection
            .selected_clip_ids
            .iter()
            .find(|clip_id| state.audio_clip_conversion_source(clip_id).is_ok())
            .cloned()
    }

    /// Audio › Audio to MIDI as the menu bar shows it: available while this
    /// build and license offer a converter and a selected audio clip can be
    /// converted. Without a converter the items say what would enable them.
    pub(super) fn audio_to_midi_menu_states(
        &self,
        cx: &gpui::App,
    ) -> Vec<crate::menu::CommandMenuState<'static>> {
        let available = self.audio_to_midi_available();
        let enabled = available && self.audio_to_midi_target(cx).is_some();
        let detail = (!available).then(|| "Professional".to_string());
        ConvertMode::ALL
            .into_iter()
            .map(|mode| crate::menu::CommandMenuState {
                command: mode.command(),
                enabled,
                detail: detail.clone(),
            })
            .collect()
    }

    /// Converts the audio clip the menu was opened on (or the selected one).
    pub(super) fn convert_clip_to_midi(&mut self, mode: ConvertMode, cx: &mut Context<Self>) {
        let Some(converter) = audio_to_midi_converter() else {
            return;
        };
        let Some(clip_id) = self.audio_to_midi_target(cx) else {
            return;
        };
        let (source, generation) = {
            let timeline = self.timeline.read(cx);
            (
                timeline.state.audio_clip_conversion_source(&clip_id),
                timeline.project_generation(),
            )
        };
        let source = match source {
            Ok(source) => source,
            Err(message) => {
                BackgroundJob::start(BackgroundTaskKind::Analysis, "Convert to MIDI").fail(message);
                return;
            }
        };
        let job = BackgroundJob::start_cancellable(
            BackgroundTaskKind::Analysis,
            format!("Convert {} to MIDI", source.clip_name),
        );
        job.set_detail(format!("Listening for {}", mode.label().to_lowercase()));
        let clip_id = source.clip_id.clone();
        let clip_name = source.clip_name.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { convert(&source, mode, &converter) })
                .await;
            let _ = this.update(cx, |layout, cx| {
                layout.finish_clip_to_midi(
                    job,
                    (&clip_id, &clip_name),
                    mode,
                    generation,
                    result,
                    cx,
                );
            });
        })
        .detach();
    }

    fn finish_clip_to_midi(
        &mut self,
        job: BackgroundJob,
        (clip_id, clip_name): (&str, &str),
        mode: ConvertMode,
        generation: u64,
        result: Result<Vec<ConvertedNote>, String>,
        cx: &mut Context<Self>,
    ) {
        if job.is_cancelled() {
            // Dropped without an outcome: recorded as stopped.
            return;
        }
        if self.timeline.read(cx).project_generation() != generation {
            job.fail("The project was closed while the clip was being converted.");
            return;
        }
        let heard = match result {
            Ok(notes) => notes,
            Err(message) => {
                job.fail(message);
                return;
            }
        };
        if heard.is_empty() {
            job.complete(Some(format!(
                "No {} heard in the clip",
                match mode {
                    ConvertMode::Drums => "hits",
                    _ => "notes",
                }
            )));
            return;
        }
        let created = self.timeline.update(cx, |timeline, cx| {
            let state = &timeline.state;
            let (source_track, clip) = state
                .find_clip(clip_id)
                .ok_or("The clip was deleted while it was being converted.")?;
            let (start, length) = (clip.start_beat, clip.duration_beats);
            let parent = source_track.parent_group_id.clone();
            let below = state
                .tracks
                .iter()
                .position(|track| track.id == source_track.id)
                .map(|index| index + 1);
            // Drum notes name instruments: they are not moved by the clip's
            // pitch shift.
            let notes = state
                .converted_midi_notes(clip_id, &heard, mode != ConvertMode::Drums)
                .unwrap_or_default();
            if notes.is_empty() {
                return Err("Nothing heard falls inside the clip.");
            }

            // The track and its clip come and go together: one step.
            let edit = timeline.begin_track_edit(TrackEditScope::track_list());
            let track_id = timeline.state.create_midi_track();
            timeline
                .state
                .set_track_name(&track_id, &track_name(clip_name, mode));
            if let Some(index) = below {
                timeline.state.reorder_track(&track_id, index);
            }
            if let Some(track) = timeline
                .state
                .tracks
                .iter_mut()
                .find(|track| track.id == track_id)
            {
                track.parent_group_id = parent;
            }
            let Some(mut clip) = timeline.state.build_midi_clip(&track_id, start, length) else {
                timeline.commit_track_edit("Convert to MIDI", edit, false, cx);
                return Err("The MIDI clip could not be made.");
            };
            clip.name = clip_name.to_string();
            let count = notes.len();
            if let ClipType::Midi {
                notes: clip_notes, ..
            } = &mut clip.clip_type
            {
                *clip_notes = notes;
            }
            timeline.state.select_track(&track_id);
            EditCommand::CreateClip {
                track_id: track_id.clone(),
                clip,
            }
            .execute(&mut timeline.state);
            timeline.commit_track_edit("Convert to MIDI", edit, false, cx);
            timeline.mark_project_changed(cx);
            cx.notify();
            Ok(count)
        });
        match created {
            Ok(count) => {
                job.complete(Some(match count {
                    1 => "1 note".to_string(),
                    n => format!("{n} notes"),
                }));
                self.mark_dirty();
                self.schedule_audio_project_sync(cx, false, "audio_to_midi");
                cx.notify();
            }
            Err(message) => job.fail(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_track_is_named_after_the_clip_and_the_mode() {
        assert_eq!(
            track_name("Vocal", ConvertMode::Melody),
            "Vocal (Melody MIDI)"
        );
        assert_eq!(track_name("  ", ConvertMode::Drums), "Audio (Drums MIDI)");
    }
}

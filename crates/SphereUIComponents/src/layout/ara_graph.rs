//! Project state → ARA document graph.
//!
//! Turns the timeline into the records [`sphere_ara_host`] needs. Pure and
//! side-effect free: it reads `TimelineState` and returns owned data, so the
//! mapping can be tested without a plug-in, an engine, or a GPUI context.

use std::collections::HashMap;
use std::path::PathBuf;

use sphere_ara_host::{
    AraAudioSourceDesc, AraBarSignature, AraClipKey, AraGraph, AraMusicalTimeline,
    AraPlaybackRegionDesc, AraPlaybackTransform, AraRegionSequenceDesc, AraSourceKey,
    AraTempoEntry, AraTrackKey,
};

use super::ara_ops::AraSessionKey;
use crate::components::timeline::timeline_state::{ClipState, ClipType, TimelineState};

/// Native shape of one audio file: sample rate, frames per channel, channels.
///
/// ARA reads a source at its own rate and channel count, never the project's, so
/// this has to come from the file rather than from the session.
pub type SourceShape = (f64, i64, i32);

/// Everything one ARA session needs from the project.
#[derive(Debug, Default)]
pub struct AraProjectView {
    pub graph: AraGraph,
    /// Where each ARA audio source reads from on disk.
    pub media_paths: HashMap<AraSourceKey, PathBuf>,
}

/// Reads a clip's source id and on-disk path, or `None` when it has neither.
fn clip_source(clip: &ClipState) -> Option<(&String, &String)> {
    let (file_id, source_path) = match &clip.clip_type {
        ClipType::Audio {
            file_id,
            source_path: Some(source_path),
        } => (file_id, source_path),
        _ => return None,
    };
    (!source_path.trim().is_empty()).then_some((file_id, source_path))
}

/// Where in the source this clip starts, in seconds.
///
/// Mirrors the engine snapshot's own trim resolution (`clip_source_offset_seconds`)
/// so the plug-in and the engine agree on which part of the file a clip covers.
fn source_offset_seconds(state: &TimelineState, clip: &ClipState) -> f64 {
    let stretch = &clip.stretch;
    if stretch.source_start_samples > 0 {
        let rate = stretch.source_sample_rate().max(1) as f64;
        stretch.source_start_samples as f64 / rate
    } else {
        // Legacy projects stored trims only as beat offsets.
        state.beats_to_seconds(clip.offset_beats.max(0.0)) as f64
    }
}

/// The project's tempo map and bar signatures, as ARA content.
///
/// ARA requires at least two strictly increasing tempo entries, so a project
/// with no tempo automation is expressed as two implicit endpoints rather than
/// as the single point a plug-in would reject.
pub fn musical_timeline(state: &TimelineState) -> AraMusicalTimeline {
    let base_bpm = state.bpm.max(1.0) as f64;
    let mut tempo: Vec<AraTempoEntry> = Vec::new();

    fn push(tempo: &mut Vec<AraTempoEntry>, state: &TimelineState, base_bpm: f64, quarter: f64) {
        let seconds = state.tempo_map.seconds_at_beat(quarter, base_bpm);
        let strictly_after = tempo.last().is_none_or(|last: &AraTempoEntry| {
            seconds > last.time_seconds && quarter > last.quarter_position
        });
        if strictly_after {
            tempo.push(AraTempoEntry {
                time_seconds: seconds,
                quarter_position: quarter,
            });
        }
    }

    push(&mut tempo, state, base_bpm, 0.0);
    for point in &state.tempo_map.points {
        let beat = point.beat as f64;
        if beat > 0.0 {
            push(&mut tempo, state, base_bpm, beat);
        }
    }
    // One quarter past the last change, so the map spans the whole timeline:
    // ARA extrapolates the final segment forward from the last pair.
    let last = tempo
        .last()
        .map(|entry| entry.quarter_position)
        .unwrap_or(0.0);
    push(&mut tempo, state, base_bpm, last + 1.0);

    let mut bars: Vec<AraBarSignature> = Vec::new();
    for point in &state.time_signature_map.points {
        let quarter = point.beat as f64;
        let strictly_after = bars
            .last()
            .is_none_or(|last: &AraBarSignature| quarter > last.quarter_position);
        if strictly_after {
            bars.push(AraBarSignature {
                numerator: (point.numerator.max(1)) as i32,
                denominator: (point.denominator.max(1)) as i32,
                quarter_position: quarter,
            });
        }
    }
    if bars.first().map(|bar| bar.quarter_position) != Some(0.0) {
        bars.insert(
            0,
            AraBarSignature {
                numerator: state.time_signature_num.max(1) as i32,
                denominator: state.time_signature_den.max(1) as i32,
                quarter_position: 0.0,
            },
        );
    }

    AraMusicalTimeline { tempo, bars }
}

/// Builds the ARA graph for one (plug-in, track) session.
///
/// Only clips bound to this plug-in on this track are included: a session
/// renders exactly the regions assigned to it, so listing a clip another
/// plug-in owns would put two renderers on the same audio.
///
/// `shape_of` resolves a media path to the file's native shape; a clip whose
/// source cannot be probed is skipped rather than described with invented
/// numbers.
pub fn project_view(
    state: &TimelineState,
    key: &AraSessionKey,
    shape_of: &mut dyn FnMut(&str) -> Option<SourceShape>,
) -> AraProjectView {
    let mut sources: HashMap<AraSourceKey, AraAudioSourceDesc> = HashMap::new();
    let mut media_paths: HashMap<AraSourceKey, PathBuf> = HashMap::new();
    let mut regions: Vec<AraPlaybackRegionDesc> = Vec::new();
    let mut sequences: Vec<AraRegionSequenceDesc> = Vec::new();
    let base_bpm = state.bpm.max(1.0) as f64;

    let Some((order, track)) = state
        .tracks
        .iter()
        .enumerate()
        .find(|(_, track)| track.id == key.track_id)
    else {
        return AraProjectView::default();
    };

    sequences.push(AraRegionSequenceDesc {
        key: AraTrackKey(track.id.clone()),
        name: track.name.clone(),
        order_index: order as i32,
        color: None,
    });

    // ARA is a track processor, so the plug-in gets every audio clip on the
    // track — not a hand-picked subset. A track bound to a different plug-in
    // contributes nothing to this session.
    if track
        .ara
        .as_ref()
        .is_none_or(|binding| binding.plugin_id != key.plugin_id)
    {
        return AraProjectView {
            graph: AraGraph {
                name: Some(track.name.clone()),
                sequences,
                ..AraGraph::default()
            },
            media_paths,
        };
    }

    for clip in &track.clips {
        let Some((file_id, source_path)) = clip_source(clip) else {
            continue;
        };
        let source_key = AraSourceKey(file_id.clone());

        if !sources.contains_key(&source_key) {
            let Some((sample_rate, frame_count, channel_count)) = shape_of(source_path) else {
                continue;
            };
            sources.insert(
                source_key.clone(),
                AraAudioSourceDesc {
                    key: source_key.clone(),
                    name: clip.name.clone(),
                    sample_rate,
                    frame_count,
                    channel_count,
                },
            );
            media_paths.insert(source_key.clone(), PathBuf::from(source_path));
        }

        // Both ends go through the tempo map so a clip over a tempo change lands
        // where the engine puts it, rather than at a fixed-tempo approximation.
        let start_in_playback = state
            .tempo_map
            .seconds_at_beat(clip.start_beat.max(0.0) as f64, base_bpm);
        let end_in_playback = state.tempo_map.seconds_at_beat(
            (clip.start_beat.max(0.0) + clip.duration_beats.max(0.0)) as f64,
            base_bpm,
        );
        let duration_in_playback = (end_in_playback - start_in_playback).max(f64::EPSILON);

        regions.push(AraPlaybackRegionDesc {
            key: AraClipKey(clip.id.clone()),
            source: source_key,
            track: AraTrackKey(track.id.clone()),
            name: clip.name.clone(),
            start_in_modification: source_offset_seconds(state, clip),
            // 1:1 with playback time, and no transformation requested: the
            // engine's own stretch path still owns non-ARA clips, and asking the
            // plug-in to stretch as well would apply the ratio twice.
            duration_in_modification: duration_in_playback,
            start_in_playback,
            duration_in_playback,
            transform: AraPlaybackTransform::NONE,
            color: None,
        });
    }

    AraProjectView {
        graph: AraGraph {
            name: Some(track.name.clone()),
            sources: sources.into_values().collect(),
            sequences,
            regions,
        },
        media_paths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::AraTrackBinding;

    fn timeline_with_ara_track() -> TimelineState {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let track_id = state.create_audio_track();
        let track = state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("just created");
        track.name = "Vocals".to_string();
        track.ara = Some(AraTrackBinding {
            plugin_id: "vst3:melodyne".to_string(),
            plugin_path: "C:/Melodyne.vst3".to_string(),
            class_id: "ABCD".to_string(),
        });
        let clip = ClipState {
            id: "clip-1".to_string(),
            name: "Take 1".to_string(),
            start_beat: 4.0,
            duration_beats: 8.0,
            source_duration_seconds: Some(4.0),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "asset-1".to_string(),
                source_path: Some("C:/take1.wav".to_string()),
            },
            muted: false,
            audio_import: Default::default(),
            stretch: Default::default(),
        };
        state.tracks[0].clips = vec![clip];
        state
    }

    fn key(state: &TimelineState) -> AraSessionKey {
        AraSessionKey {
            plugin_id: "vst3:melodyne".to_string(),
            track_id: state.tracks[0].id.clone(),
        }
    }

    #[test]
    fn every_audio_clip_on_an_ara_track_becomes_a_playback_region() {
        let state = timeline_with_ara_track();
        let mut shape = |_: &str| Some((48_000.0, 192_000, 2));
        let view = project_view(&state, &key(&state), &mut shape);

        assert!(view.graph.validate().is_ok());
        assert_eq!(view.graph.regions.len(), 1);
        assert_eq!(view.graph.sources.len(), 1);
        assert_eq!(view.graph.sequences.len(), 1);

        let region = &view.graph.regions[0];
        // 4 beats at 120 BPM = 2 s in, 8 beats = 4 s long.
        assert!((region.start_in_playback - 2.0).abs() < 1e-9);
        assert!((region.duration_in_playback - 4.0).abs() < 1e-9);
        // Nothing was trimmed, so the region starts at the head of the source.
        assert!((region.start_in_modification - 0.0).abs() < 1e-9);
        assert_eq!(view.media_paths.len(), 1);
    }

    #[test]
    fn a_track_bound_to_another_plugin_contributes_nothing() {
        let mut state = timeline_with_ara_track();
        state.tracks[0].ara.as_mut().unwrap().plugin_id = "vst3:other".to_string();
        let mut shape = |_: &str| Some((48_000.0, 192_000, 2));
        let view = project_view(&state, &key(&state), &mut shape);
        assert!(view.graph.regions.is_empty());
        // The sequence is still declared: the session owns the track either way.
        assert_eq!(view.graph.sequences.len(), 1);
    }

    #[test]
    fn an_unprobeable_source_is_skipped_rather_than_guessed() {
        let state = timeline_with_ara_track();
        let mut shape = |_: &str| None;
        let view = project_view(&state, &key(&state), &mut shape);
        assert!(view.graph.regions.is_empty());
        assert!(view.graph.sources.is_empty());
    }

    #[test]
    fn a_flat_project_still_yields_a_valid_two_point_tempo_map() {
        let state = timeline_with_ara_track();
        let timeline = musical_timeline(&state);
        assert!(
            timeline.validate().is_ok(),
            "a project with no tempo automation must still satisfy ARA"
        );
        assert!(timeline.tempo.len() >= 2);
        assert_eq!(
            timeline.bars.first().map(|bar| bar.quarter_position),
            Some(0.0)
        );
    }
}

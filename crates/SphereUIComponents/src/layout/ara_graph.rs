//! Project state → ARA document graph.
//!
//! Turns the timeline into the records [`sphere_ara_host`] needs. Pure and
//! side-effect free: it reads `TimelineState` and returns owned data, so the
//! mapping can be tested without a plug-in, an engine, or a GPUI context.

use std::collections::HashMap;
use std::path::PathBuf;

use sphere_ara_host::{
    AraAudioSourceDesc, AraBarSignature, AraClipKey, AraGraph, AraKeySignature, AraMusicalTimeline,
    AraPlaybackRegionDesc, AraPlaybackTransform, AraRegionSequenceDesc, AraSourceKey,
    AraTempoEntry, AraTrackKey,
};

use super::ara_ops::AraSessionKey;
use crate::components::timeline::timeline_state::{
    ClipState, ClipType, MidiScale, ScaleKind, TimelineState,
};

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
    /// Clips of this session left out of the graph because their media could
    /// not be probed (offline, moved, unreadable), with the source each one
    /// reads. A saved document may hold state for them that cannot land yet.
    pub offline: Vec<(AraClipKey, AraSourceKey)>,
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

/// How far a mode's key signature sits from its tonic on the circle of fifths.
///
/// A key's accidental count is its tonic's fifths index plus this offset: A
/// minor is 3 − 3 = 0, D Dorian 2 − 2 = 0, F Lydian −1 + 1 = 0. Pentatonic
/// and the minor variants count as their parent major or natural minor.
fn mode_fifths_offset(kind: ScaleKind) -> i32 {
    match kind {
        ScaleKind::Chromatic | ScaleKind::Major | ScaleKind::MajorPentatonic => 0,
        ScaleKind::Lydian => 1,
        ScaleKind::Mixolydian => -1,
        ScaleKind::Dorian => -2,
        ScaleKind::NaturalMinor
        | ScaleKind::HarmonicMinor
        | ScaleKind::MelodicMinor
        | ScaleKind::MinorPentatonic => -3,
        ScaleKind::Phrygian => -4,
        ScaleKind::Locrian => -5,
    }
}

/// The tonic's circle-of-fifths index (C = 0, G = 1, F = −1), spelled with the
/// fewest accidentals `kind` allows: D♭ major (−5) but C♯ minor (7).
///
/// A pitch class has one sharp-side and one flat-side index, twelve apart. A
/// tie — six accidentals either way, as in F♯/G♭ major or D♯/E♭ minor — keeps
/// the sharp spelling Futureboard's own key readout uses.
fn root_fifths(pitch_class: u8, kind: ScaleKind) -> i32 {
    let sharp_side = (i32::from(pitch_class) * 7).rem_euclid(12);
    let flat_side = sharp_side - 12;
    let offset = mode_fifths_offset(kind);
    if (flat_side + offset).abs() < (sharp_side + offset).abs() {
        flat_side
    } else {
        sharp_side
    }
}

/// Note name for a circle-of-fifths index, with the U+266F / U+266D signs ARA
/// requires in content names.
fn fifths_note_name(fifths: i32) -> String {
    const LETTERS: [char; 7] = ['F', 'C', 'G', 'D', 'A', 'E', 'B'];
    let letter = LETTERS[(fifths + 1).rem_euclid(7) as usize];
    let accidentals = (fifths + 1).div_euclid(7);
    let sign = if accidentals < 0 {
        '\u{266D}'
    } else {
        '\u{266F}'
    };
    let mut name = String::from(letter);
    name.extend(std::iter::repeat_n(
        sign,
        accidentals.unsigned_abs() as usize,
    ));
    name
}

/// The project key as one ARA key signature, valid from the start of the
/// timeline (and, per ARA, before it). `None` when there is no key.
///
/// The name is the key's label ("D♭ Major"), spelled like `root_fifths` so the
/// two never disagree.
fn key_signature(key: MidiScale) -> Option<AraKeySignature> {
    if key.kind == ScaleKind::Chromatic {
        return None;
    }
    let root_fifths = root_fifths(key.root.pitch_class(), key.kind);
    let mut intervals = [false; 12];
    for &interval in key.kind.intervals() {
        intervals[usize::from(interval) % 12] = true;
    }
    Some(AraKeySignature {
        root_fifths,
        intervals,
        name: Some(format!(
            "{} {}",
            fifths_note_name(root_fifths),
            key.kind.label()
        )),
        quarter_position: 0.0,
    })
}

/// The project's tempo map, bar signatures and key, as ARA content.
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

    AraMusicalTimeline {
        tempo,
        bars,
        keys: state
            .project_key
            .and_then(key_signature)
            .into_iter()
            .collect(),
    }
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
    let mut offline: Vec<(AraClipKey, AraSourceKey)> = Vec::new();
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
            offline,
        };
    }

    for clip in &track.clips {
        let Some((file_id, source_path)) = clip_source(clip) else {
            continue;
        };
        let source_key = AraSourceKey(file_id.clone());

        if !sources.contains_key(&source_key) {
            let Some((sample_rate, frame_count, channel_count)) = shape_of(source_path) else {
                offline.push((AraClipKey(clip.id.clone()), source_key));
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
        offline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{AraTrackBinding, ScaleRoot};

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

    /// The eager copy into a saved project moves a just-dropped clip to the
    /// copy's id (`audio_import::retarget_imported_clips`); a clip that
    /// shares the dropped path and was not part of that drop keeps it. The
    /// graph then names two sources, each clip on its own, which is what lets
    /// the host rebuild exactly the moved clip's region and modification on
    /// the new source instead of updating it in place over the old one.
    #[test]
    fn a_clip_moved_to_its_project_copy_reads_a_source_of_its_own() {
        let mut state = timeline_with_ara_track();
        let mut moved = state.tracks[0].clips[0].clone();
        moved.id = "clip-2".to_string();
        moved.clip_type = ClipType::Audio {
            file_id: "Assets/Audio/take1-1.wav".to_string(),
            source_path: Some("/project/Assets/Audio/take1-1.wav".to_string()),
        };
        state.tracks[0].clips.push(moved);
        let mut shape = |_: &str| Some((48_000.0, 192_000, 2));
        let view = project_view(&state, &key(&state), &mut shape);

        assert!(view.graph.validate().is_ok());
        assert_eq!(view.graph.sources.len(), 2);
        let source_of = |clip: &str| {
            view.graph
                .regions
                .iter()
                .find(|region| region.key.as_str() == clip)
                .map(|region| region.source.as_str().to_owned())
        };
        assert_eq!(source_of("clip-1").as_deref(), Some("asset-1"));
        assert_eq!(
            source_of("clip-2").as_deref(),
            Some("Assets/Audio/take1-1.wav")
        );
        assert_eq!(
            view.media_paths
                .get(&AraSourceKey::from("Assets/Audio/take1-1.wav")),
            Some(&PathBuf::from("/project/Assets/Audio/take1-1.wav"))
        );
    }

    #[test]
    fn an_unprobeable_source_is_skipped_rather_than_guessed() {
        let state = timeline_with_ara_track();
        let mut shape = |_: &str| None;
        let view = project_view(&state, &key(&state), &mut shape);
        assert!(view.graph.regions.is_empty());
        assert!(view.graph.sources.is_empty());
        // ...and named, so a saved document holding its state waits for it
        // instead of being restored into a graph without it.
        assert_eq!(
            view.offline,
            vec![(AraClipKey::from("clip-1"), AraSourceKey::from("asset-1"))]
        );
    }

    /// Only a clip whose media could not be probed is offline: one that is in
    /// the graph is not, and neither is one with no media at all.
    #[test]
    fn only_clips_whose_media_cannot_be_probed_are_offline() {
        let mut state = timeline_with_ara_track();
        let mut gone = state.tracks[0].clips[0].clone();
        gone.id = "clip-2".to_string();
        gone.clip_type = ClipType::Audio {
            file_id: "Assets/Audio/gone.wav".to_string(),
            source_path: Some("/unmounted/gone.wav".to_string()),
        };
        let mut unwritten = state.tracks[0].clips[0].clone();
        unwritten.id = "clip-3".to_string();
        unwritten.clip_type = ClipType::Audio {
            file_id: "take".to_string(),
            source_path: None,
        };
        state.tracks[0].clips.push(gone);
        state.tracks[0].clips.push(unwritten);
        let mut shape = |path: &str| (!path.starts_with("/unmounted")).then_some((48_000.0, 10, 1));
        let view = project_view(&state, &key(&state), &mut shape);
        assert_eq!(view.graph.regions.len(), 1);
        assert_eq!(
            view.offline,
            vec![(
                AraClipKey::from("clip-2"),
                AraSourceKey::from("Assets/Audio/gone.wav")
            )]
        );
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

    fn timeline_in(key: Option<MidiScale>) -> AraMusicalTimeline {
        let mut state = timeline_with_ara_track();
        state.project_key = key;
        musical_timeline(&state)
    }

    #[test]
    fn no_project_key_publishes_no_key() {
        let timeline = timeline_in(None);
        assert!(timeline.keys.is_empty());
        assert!(timeline.validate().is_ok());
    }

    #[test]
    fn the_key_changes_only_the_keys_of_the_timeline() {
        let without = timeline_in(None);
        let with = timeline_in(Some(MidiScale::new(ScaleRoot::A, ScaleKind::NaturalMinor)));
        assert_eq!(with.tempo, without.tempo);
        assert_eq!(with.bars, without.bars);
        assert_eq!(with.keys.len(), 1);
    }

    #[test]
    fn a_minor_is_one_key_at_the_start_with_no_accidentals() {
        let timeline = timeline_in(Some(MidiScale::new(ScaleRoot::A, ScaleKind::NaturalMinor)));
        assert!(timeline.validate().is_ok());
        let key = &timeline.keys[0];
        assert_eq!(key.root_fifths, 3);
        assert_eq!(key.quarter_position, 0.0);
        assert_eq!(
            key.intervals,
            [true, false, true, true, false, true, false, true, true, false, true, false]
        );
        assert_eq!(key.name.as_deref(), Some("A Natural Minor"));
    }

    #[test]
    fn black_key_roots_take_the_spelling_with_fewer_accidentals() {
        let key = |root, kind| key_signature(MidiScale::new(root, kind)).expect("a real key");
        assert_eq!(key(ScaleRoot::F, ScaleKind::Major).root_fifths, -1);
        let d_flat = key(ScaleRoot::CSharp, ScaleKind::Major);
        assert_eq!(d_flat.root_fifths, -5);
        assert_eq!(d_flat.name.as_deref(), Some("D\u{266D} Major"));
        let c_sharp_minor = key(ScaleRoot::CSharp, ScaleKind::NaturalMinor);
        assert_eq!(c_sharp_minor.root_fifths, 7);
        assert_eq!(
            c_sharp_minor.name.as_deref(),
            Some("C\u{266F} Natural Minor")
        );
        // D♯/E♭ Dorian: E♭ has five flats, D♯ would need seven sharps.
        assert_eq!(key(ScaleRoot::DSharp, ScaleKind::Dorian).root_fifths, -3);
        // Six either way keeps the sharp spelling of the key readout.
        assert_eq!(key(ScaleRoot::FSharp, ScaleKind::Major).root_fifths, 6);
    }

    /// Every root in every scale a project key can have.
    #[test]
    fn every_project_key_maps_to_a_valid_ara_key_signature() {
        // Tonic fifths index per pitch class, C upwards.
        const MAJOR: [i32; 12] = [0, -5, 2, -3, 4, -1, 6, 1, -4, 3, -2, 5];
        const MINOR: [i32; 12] = [0, 7, 2, 9, 4, -1, 6, 1, 8, 3, -2, 5];

        for root in ScaleRoot::ALL {
            for kind in MidiScale::KEY_KINDS {
                let timeline = timeline_in(Some(MidiScale::new(root, kind)));
                assert!(timeline.validate().is_ok(), "{root:?} {kind:?}");
                assert_eq!(timeline.keys.len(), 1);
                let key = &timeline.keys[0];
                let pitch_class = root.pitch_class();

                // The index names this pitch class: seven semitones per fifth.
                assert_eq!(
                    (key.root_fifths * 7).rem_euclid(12),
                    i32::from(pitch_class),
                    "{root:?} {kind:?}"
                );
                // No other spelling of the tonic needs fewer accidentals.
                let accidentals = (key.root_fifths + mode_fifths_offset(kind)).abs();
                assert!(accidentals <= 6, "{root:?} {kind:?}: {accidentals}");
                if accidentals == 6 {
                    assert!(key.root_fifths >= 0, "ties keep the sharp spelling");
                }

                let expected: Vec<usize> = kind.intervals().iter().map(|&i| i as usize).collect();
                let used: Vec<usize> = (0..12).filter(|&i| key.intervals[i]).collect();
                assert_eq!(used, expected, "{root:?} {kind:?}");

                let name = key.name.as_deref().expect("every key is named");
                assert!(name.ends_with(kind.label()), "{name}");
                assert!(!name.contains('#'), "ARA wants U+266F, got {name}");
                assert!(name.starts_with(&fifths_note_name(key.root_fifths)));
                assert_eq!(key.quarter_position, 0.0);

                let table = match kind {
                    ScaleKind::Major | ScaleKind::MajorPentatonic => Some(MAJOR),
                    ScaleKind::NaturalMinor
                    | ScaleKind::HarmonicMinor
                    | ScaleKind::MelodicMinor
                    | ScaleKind::MinorPentatonic => Some(MINOR),
                    _ => None,
                };
                if let Some(table) = table {
                    assert_eq!(
                        key.root_fifths, table[pitch_class as usize],
                        "{root:?} {kind:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn fifths_indices_are_named_with_ara_accidentals() {
        assert_eq!(fifths_note_name(0), "C");
        assert_eq!(fifths_note_name(-1), "F");
        assert_eq!(fifths_note_name(-2), "B\u{266D}");
        assert_eq!(fifths_note_name(6), "F\u{266F}");
        assert_eq!(fifths_note_name(-6), "G\u{266D}");
        assert_eq!(fifths_note_name(-8), "F\u{266D}");
        assert_eq!(fifths_note_name(13), "F\u{266F}\u{266F}");
    }
}

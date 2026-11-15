//! The step view's Sampler Rack: one row per instrument track.
//!
//! Where Drum Sampler mode reads the lanes of one clip (one pitch per pad of
//! the kit on the clip's own track), the rack reads several tracks at once.
//! A rack is a track Group: every Instrument track in the group of the track
//! being edited is a row, in track order, each with its own instrument,
//! mixer strip and clip. A row's steps are the notes of that track's MIDI
//! clip over the same bars as the clip being edited, at the pitch the row
//! plays. Nothing is stored for the rack itself: membership is the group,
//! the pattern is the clips, so it saves, undoes and plays like any tracks.

use crate::components::timeline::timeline_state::{
    ClipState, ClipType, MidiNoteState, TimelineState, TrackType,
};

/// The note a row writes when its clip has none yet: middle C, which a
/// sampler plays at the sample's own pitch.
pub(crate) const ROW_PITCH: u8 = 60;

/// How close a clip's start must be to the pattern's to be the row's clip,
/// in beats.
const SAME_START_BEATS: f32 = 0.001;

/// One row of the rack.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RackRow {
    pub track_id: String,
    pub name: String,
    pub color: gpui::Rgba,
    /// The row's clip over the pattern's bars, if it has one yet.
    pub clip_id: Option<String>,
    /// The note the row's steps are written at.
    pub pitch: u8,
    pub muted: bool,
    pub solo: bool,
    /// Fader position, `0.0..=1.0` (see `TrackState::volume`).
    pub volume: f32,
    pub pan: f32,
    /// The track's instrument insert, for its editor and sample.
    pub instrument_insert_id: Option<String>,
}

/// The pattern the rack edits: where it sits and how long it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RackPattern {
    pub start_beat: f32,
    pub length_beats: f32,
}

/// The rack around `clip_id`: its pattern and its rows. `None` when the clip
/// is not a MIDI clip on an Instrument track.
pub(crate) fn rack_for_clip(
    state: &TimelineState,
    clip_id: &str,
) -> Option<(RackPattern, Vec<RackRow>)> {
    let (track, clip) = state.find_clip(clip_id)?;
    if !plays_notes(track.track_type) || !matches!(clip.clip_type, ClipType::Midi { .. }) {
        return None;
    }
    let pattern = RackPattern {
        start_beat: clip.start_beat,
        length_beats: clip.duration_beats,
    };
    let members: Vec<_> = match track.parent_group_id.as_deref() {
        Some(group) => state
            .tracks
            .iter()
            .filter(|t| plays_notes(t.track_type) && t.parent_group_id.as_deref() == Some(group))
            .collect(),
        None => vec![track],
    };
    let rows = members
        .into_iter()
        .map(|member| {
            let clip = if member.id == track.id {
                Some(clip)
            } else {
                row_clip(&member.clips, pattern)
            };
            RackRow {
                track_id: member.id.clone(),
                name: member.name.clone(),
                color: member.color,
                clip_id: clip.map(|clip| clip.id.clone()),
                pitch: clip.and_then(row_pitch).unwrap_or(ROW_PITCH),
                muted: member.muted,
                solo: member.solo,
                volume: member.volume,
                pan: member.pan,
                instrument_insert_id: member.instrument_plugin_instance_id.clone(),
            }
        })
        .collect();
    Some((pattern, rows))
}

/// Tracks a rack row can be: the ones whose clips are notes for an
/// instrument (an Instrument track's own, or a MIDI track's).
fn plays_notes(track_type: TrackType) -> bool {
    matches!(track_type, TrackType::Instrument | TrackType::Midi)
}

/// A track's MIDI clip that starts where the pattern does.
fn row_clip(clips: &[ClipState], pattern: RackPattern) -> Option<&ClipState> {
    clips.iter().find(|clip| {
        matches!(clip.clip_type, ClipType::Midi { .. })
            && (clip.start_beat - pattern.start_beat).abs() <= SAME_START_BEATS
    })
}

/// The pitch a row's clip plays most, lowest on a tie: what its steps read
/// and write.
fn row_pitch(clip: &ClipState) -> Option<u8> {
    let ClipType::Midi { notes, .. } = &clip.clip_type else {
        return None;
    };
    most_played_pitch(notes)
}

pub(crate) fn most_played_pitch(notes: &[MidiNoteState]) -> Option<u8> {
    let mut counts = [0usize; 128];
    for note in notes {
        counts[usize::from(note.pitch.min(127))] += 1;
    }
    let (pitch, count) = counts
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(&a.0)))?;
    (*count > 0).then_some(pitch as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{CreateTrackOptions, InputMonitorMode};

    fn note(pitch: u8, start: f32) -> MidiNoteState {
        MidiNoteState::new(pitch, start, 0.25, 100)
    }

    fn track(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: state.track_color_for_index(state.tracks.len()),
            volume: 0.8,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    /// Kick and Snare in a group (with an audio track and a nested folder
    /// that are not rows), Hat outside it. The pattern is Kick's clip at
    /// bar 2; Snare has a clip there playing D.
    fn rack() -> (TimelineState, String, [String; 3]) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let group = track(&mut state, TrackType::Group, "Rack");
        let kick = track(&mut state, TrackType::Instrument, "Kick");
        let snare = track(&mut state, TrackType::Instrument, "Snare");
        let audio = track(&mut state, TrackType::Audio, "Loop");
        let hat = track(&mut state, TrackType::Instrument, "Hat");
        for member in [&kick, &snare, &audio] {
            assert!(state.assign_track_to_group(member, &group));
        }
        let pattern = state.create_midi_clip(&kick, 4.0, 4.0).expect("clip");
        let snare_clip = state.create_midi_clip(&snare, 4.0, 4.0).expect("clip");
        state.add_midi_note(&snare_clip, 62, 0.0, 0.25, 100);
        // Another of Snare's clips, elsewhere: not this pattern's.
        state.create_midi_clip(&snare, 12.0, 4.0).expect("clip");
        (state, pattern, [kick, snare, hat])
    }

    #[test]
    fn the_rows_are_the_note_tracks_of_the_edited_tracks_group() {
        let (state, pattern, [kick, snare, _hat]) = rack();
        let (span, rows) = rack_for_clip(&state, &pattern).expect("rack");
        assert_eq!(
            span,
            RackPattern {
                start_beat: 4.0,
                length_beats: 4.0
            }
        );
        let ids: Vec<&str> = rows.iter().map(|row| row.track_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![kick.as_str(), snare.as_str()],
            "audio and outside tracks are not rows"
        );
        assert_eq!(rows[0].clip_id.as_deref(), Some(pattern.as_str()));
        // Snare's row is its clip at the same bars, at the pitch it plays.
        let snare_row = &rows[1];
        assert!(snare_row.clip_id.is_some());
        assert_eq!(snare_row.pitch, 62);
        assert_eq!(snare_row.name, "Snare");
    }

    /// A row with no clip over the pattern's bars has none (the editor makes
    /// one when a step is painted) and writes middle C.
    #[test]
    fn a_row_without_a_clip_at_the_pattern_has_none_yet() {
        let (mut state, pattern, [_kick, snare, _]) = rack();
        let ids: Vec<String> = state
            .find_track(&snare)
            .unwrap()
            .clips
            .iter()
            .map(|clip| clip.id.clone())
            .collect();
        for id in ids {
            if state
                .find_clip(&id)
                .is_some_and(|(_, clip)| clip.start_beat == 4.0)
            {
                state.delete_clip(&id);
            }
        }
        let (_, rows) = rack_for_clip(&state, &pattern).unwrap();
        assert_eq!(rows[1].clip_id, None);
        assert_eq!(rows[1].pitch, ROW_PITCH);
    }

    /// A track in no group is a rack of one.
    #[test]
    fn a_track_outside_any_group_is_a_rack_of_one() {
        let (mut state, _, [_, _, hat]) = rack();
        let clip = state.create_midi_clip(&hat, 0.0, 4.0).unwrap();
        let (_, rows) = rack_for_clip(&state, &clip).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].track_id, hat);
    }

    #[test]
    fn a_row_reads_the_pitch_its_clip_plays_most() {
        assert_eq!(
            most_played_pitch(&[note(62, 0.0), note(60, 1.0), note(62, 2.0)]),
            Some(62)
        );
        // A tie goes to the lower note; nothing played is no pitch.
        assert_eq!(most_played_pitch(&[note(64, 0.0), note(48, 1.0)]), Some(48));
        assert_eq!(most_played_pitch(&[]), None);
    }
}

//! Solo resolution across routes, MIDI sources and VSTi output children.

use super::*;

/// True when any insert on `source_track_index` routes a VSTi multi-out child
/// strip that is currently soloed. A soloed child keeps its parent instrument
/// track audible/scheduled even though the parent itself is not soloed. Shared
/// by the render scheduling predicate and the scoped mute/solo note-off.
#[inline]
pub(crate) fn has_soloed_vsti_output_child(
    runtime: &RuntimeProject,
    source_track_index: usize,
) -> bool {
    let Some(source_track) = runtime.tracks.get(source_track_index) else {
        return false;
    };
    source_track.inserts.iter().any(|insert| {
        insert.vsti_output_children.iter().any(|child| {
            child
                .dest_track_index
                .and_then(|idx| runtime.tracks.get(idx))
                .is_some_and(|track| track.solo)
        })
    })
}

/// True when the output of `source_track_index` flows, through any number of
/// routing tracks, into one that is soloed. Soloing a group or bus is a solo
/// of what plays through it: a folder's members stay audible under their
/// soloed folder, and a folder inside a soloed folder passes its members on.
/// Follows the indices resolved by [`RuntimeProject::resolve_indices`], at
/// most once around the track list, so a routing loop cannot hang the audio
/// thread; no search, no allocation.
#[inline]
pub(crate) fn has_soloed_route_target(runtime: &RuntimeProject, source_track_index: usize) -> bool {
    let mut current = source_track_index;
    for _ in 0..runtime.tracks.len() {
        let Some(next) = runtime
            .tracks
            .get(current)
            .and_then(|track| track.output_track_index)
        else {
            return false;
        };
        let Some(target) = runtime.tracks.get(next) else {
            return false;
        };
        if next == current || !crate::audio_graph::is_routing_track_type(&target.track_type) {
            return false;
        }
        if target.solo {
            return true;
        }
        current = next;
    }
    false
}

/// True when solo, though the track itself is not soloed, still lets it play:
/// one of its VSTi channels is soloed ([`has_soloed_vsti_output_child`]), a
/// group or bus it plays through is ([`has_soloed_route_target`]), or a MIDI
/// track that plays it is ([`has_soloed_midi_source`]).
#[inline]
pub(crate) fn solo_keeps_source_audible(runtime: &RuntimeProject, track_index: usize) -> bool {
    has_soloed_vsti_output_child(runtime, track_index)
        || has_soloed_route_target(runtime, track_index)
        || has_soloed_midi_source(runtime, track_index)
}

/// True when a MIDI track routed into instrument track `track_index` is
/// soloed: soloing that MIDI track must keep the instrument it plays audible.
/// Scans the (few) MIDI schedules and their sources; no allocation.
#[inline]
pub(crate) fn has_soloed_midi_source(runtime: &RuntimeProject, track_index: usize) -> bool {
    runtime
        .midi_tracks
        .iter()
        .filter(|mt| mt.track_index == Some(track_index))
        .any(|mt| any_source_soloed(&runtime.tracks, &mt.sources))
}

pub(super) fn any_source_soloed(tracks: &[RuntimeTrack], sources: &[RuntimeMidiSource]) -> bool {
    sources
        .iter()
        .filter_map(|s| s.track_index.and_then(|ix| tracks.get(ix)))
        .any(|t| t.solo)
}

/// Whether a note from `source` (an index into `sources`, or the instrument
/// track's own clip when `None`) must not start: its MIDI track is muted
/// (automation included), or another MIDI track of the same instrument is
/// soloed and it is not. Soloing the instrument track itself keeps every
/// source playing. Audio-thread safe: index lookups only.
#[inline]
pub(super) fn midi_source_silenced(
    tracks: &[RuntimeTrack],
    sources: &[RuntimeMidiSource],
    source: Option<u16>,
    instrument_ix: usize,
    beat: f64,
) -> bool {
    if sources.is_empty() {
        return false;
    }
    let source_track = source
        .and_then(|s| sources.get(usize::from(s)))
        .and_then(|s| s.track_index)
        .and_then(|ix| tracks.get(ix));
    if let Some(track) = source_track {
        if track
            .automation_values_at_beat(beat)
            .muted
            .unwrap_or(track.muted)
        {
            return true;
        }
    }
    let instrument_soloed = tracks.get(instrument_ix).is_some_and(|t| t.solo);
    let own_solo = match source_track {
        Some(track) => track.solo,
        None => instrument_soloed,
    };
    !own_solo && !instrument_soloed && any_source_soloed(tracks, sources)
}

/// True when `child_track_index` is a VSTi multi-out child ("Out Ch") strip
/// whose parent instrument track is soloed. Soloing the main VSTi track is a
/// solo of that instrument as a whole, so every separate-output channel it
/// feeds stays audible — the inverse of [`has_soloed_vsti_output_child`], which
/// keeps the parent scheduled when one of its channels is soloed on its own.
/// Reads the index resolved by [`RuntimeProject::resolve_indices`]; no search.
#[inline]
pub(crate) fn has_soloed_vsti_output_parent(
    runtime: &RuntimeProject,
    child_track_index: usize,
) -> bool {
    runtime
        .vsti_output_parent_indices
        .get(child_track_index)
        .copied()
        .flatten()
        .and_then(|parent_index| runtime.tracks.get(parent_index))
        .is_some_and(|parent| parent.solo)
}

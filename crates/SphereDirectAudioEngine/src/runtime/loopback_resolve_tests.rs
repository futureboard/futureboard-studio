//! Track loopback: one track's output resolved as another track's input.
//!
//! The resolution is where this feature is either right or a footgun — a stale
//! index reads somebody else's audio, and a self-reference is a feedback loop —
//! so it is pinned here rather than left to the render path's own guard.

use super::*;
use crate::types::{EngineRoutingSnapshot, EngineTrackSnapshot};

fn track(id: &str, track_type: &str) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: track_type.to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: true,
        input_source: Default::default(),
        preview_mode: "stereo".to_string(),
        output_track_id: None,
        inserts: Vec::new(),
        sends: Vec::new(),
        automation_lanes: Vec::new(),
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
    }
}

/// `destination`'s input names `source` through a `trk:` device id.
fn snapshot_with_route(source: &str, destination: &str) -> EngineProjectSnapshot {
    let mut piano = track("piano", "instrument");
    piano.id = "piano".to_string();
    let mut audio = track("audio-1", "audio");
    audio.input_source = crate::types::EngineTrackInputSourceSnapshot {
        device_id: Some(crate::loopback::loopback_device_id(source)),
        channels: vec![0, 1],
    };
    audio.id = destination.to_string();
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "loopback".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks: vec![piano, audio, track("master", "master")],
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: 48_000,
            buffer_size: 512,
        },
    }
}

fn resolved(snapshot: &EngineProjectSnapshot) -> RuntimeProject {
    let mut cache = HashMap::new();
    let mut runtime =
        RuntimeProject::build(snapshot, 48_000, &mut cache, None, true).expect("build");
    runtime.resolve_loopback_inputs(snapshot);
    runtime
}

#[test]
fn an_instrument_track_resolves_to_its_index_and_starts_publishing() {
    let snapshot = snapshot_with_route("piano", "audio-1");
    let runtime = resolved(&snapshot);

    assert_eq!(
        runtime.tracks[1].input_source,
        RuntimeTrackInputSource::Loopback {
            source_index: 0,
            mode: crate::jam_bus::JamChannelMode::Stereo,
        }
    );
    // The source keeps its block only because something reads it.
    assert!(runtime.tracks[0].loopback_publish);
    assert!(!runtime.tracks[1].loopback_publish);
    assert!(!runtime.tracks[2].loopback_publish);
}

/// A route to a track that is gone captures nothing and says so. Falling
/// back to the hardware channels the route carries would put the interface
/// into a track the user pointed at an instrument.
#[test]
fn a_route_to_a_missing_track_resolves_to_nothing() {
    let snapshot = snapshot_with_route("deleted-track", "audio-1");
    let runtime = resolved(&snapshot);

    assert_eq!(
        runtime.tracks[1].input_source,
        RuntimeTrackInputSource::None
    );
    assert!(runtime.tracks.iter().all(|track| !track.loopback_publish));
}

/// A track pointed at itself is a feedback loop nobody asked for, and it is
/// refused here rather than left to the render path to notice.
#[test]
fn a_track_cannot_loop_back_its_own_output() {
    let snapshot = snapshot_with_route("audio-1", "audio-1");
    let runtime = resolved(&snapshot);

    assert_eq!(
        runtime.tracks[1].input_source,
        RuntimeTrackInputSource::None
    );
    assert!(runtime.tracks.iter().all(|track| !track.loopback_publish));
}

/// A return (the WDW return Rodhareist's wet/dry/wet split creates is one)
/// is a source like any other: a fresh audio track can record it.
#[test]
fn a_return_track_is_a_loopback_source() {
    let mut snapshot = snapshot_with_route("wdw", "audio-1");
    snapshot.tracks.insert(2, track("wdw", "return"));
    let runtime = resolved(&snapshot);

    assert_eq!(
        runtime.tracks[1].input_source,
        RuntimeTrackInputSource::Loopback {
            source_index: 2,
            mode: crate::jam_bus::JamChannelMode::Stereo,
        }
    );
    assert!(runtime.tracks[2].loopback_publish);
}

/// A track that already feeds the source — here through its output into
/// the return — would hear its own output a block later, forever. That is
/// feedback, and it is refused just like a route to itself.
#[test]
fn a_track_cannot_loop_back_a_bus_it_feeds() {
    let mut snapshot = snapshot_with_route("wdw", "audio-1");
    snapshot.tracks.insert(2, track("wdw", "return"));
    snapshot.tracks[1].output_track_id = Some("wdw".to_string());
    let runtime = resolved(&snapshot);

    assert_eq!(runtime.tracks[1].output_track_index, Some(2));
    assert_eq!(
        runtime.tracks[1].input_source,
        RuntimeTrackInputSource::None
    );
    assert!(runtime.tracks.iter().all(|track| !track.loopback_publish));
}

/// Two tracks reading each other are a loop too, even with no bus between
/// them: each would carry the other's output back into itself.
#[test]
fn two_tracks_cannot_loop_back_each_other() {
    let mut snapshot = snapshot_with_route("piano", "audio-1");
    snapshot.tracks[0].input_source = crate::types::EngineTrackInputSourceSnapshot {
        device_id: Some(crate::loopback::loopback_device_id("audio-1")),
        channels: vec![0, 1],
    };
    let runtime = resolved(&snapshot);

    assert_eq!(
        runtime.tracks[0].input_source,
        RuntimeTrackInputSource::None
    );
    assert_eq!(
        runtime.tracks[1].input_source,
        RuntimeTrackInputSource::None
    );
}

/// Re-resolving after the route is dropped has to stop the source
/// publishing, or every track that was ever a source keeps copying its
/// block for the rest of the session.
#[test]
fn dropping_the_route_stops_the_source_publishing() {
    let mut snapshot = snapshot_with_route("piano", "audio-1");
    let mut runtime = resolved(&snapshot);
    assert!(runtime.tracks[0].loopback_publish);

    snapshot.tracks[1].input_source = Default::default();
    runtime.resolve_loopback_inputs(&snapshot);

    assert!(!runtime.tracks[0].loopback_publish);
}

/// A mono destination folds the source rather than reading a channel the
/// pair does not have.
#[test]
fn the_channel_mapping_follows_the_connections_bound_channels() {
    let mut snapshot = snapshot_with_route("piano", "audio-1");
    snapshot.tracks[1].input_source.channels = vec![0];
    let runtime = resolved(&snapshot);

    assert_eq!(
        runtime.tracks[1].input_source,
        RuntimeTrackInputSource::Loopback {
            source_index: 0,
            mode: crate::jam_bus::JamChannelMode::Left,
        }
    );
}

use super::*;
use crate::types::{EngineRoutingSnapshot, EngineTrackSnapshot};

fn track_snapshot(id: &str, track_type: &str) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: track_type.to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: false,
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

fn two_track_snapshot(sample_rate: u32) -> EngineProjectSnapshot {
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "pdc-reset".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate,
        tracks: vec![
            track_snapshot("audio-1", "audio"),
            track_snapshot("master", "master"),
        ],
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate,
            buffer_size: 512,
        },
    }
}

/// A soloed folder is a solo of what plays through it, through a folder
/// inside it too; a track outside it stays silenced.
#[test]
fn a_soloed_folder_keeps_the_tracks_playing_through_it_audible() {
    let routed = |id: &str, kind: &str, to: Option<&str>| EngineTrackSnapshot {
        output_track_id: to.map(str::to_string),
        ..track_snapshot(id, kind)
    };
    let mut snapshot = two_track_snapshot(48_000);
    snapshot.tracks = vec![
        routed("outer", "group", None),
        routed("inner", "group", Some("outer")),
        routed("kick", "audio", Some("inner")),
        routed("bass", "audio", Some("outer")),
        routed("keys", "audio", None),
        track_snapshot("master", "master"),
    ];
    snapshot.tracks[0].solo = true;
    let mut cache = HashMap::new();
    let runtime = RuntimeProject::build(&snapshot, 48_000, &mut cache, None, true).expect("build");
    assert!(runtime.has_solo);
    let index = |id: &str| runtime.tracks.iter().position(|t| t.id == id).unwrap();
    assert!(solo_keeps_source_audible(&runtime, index("kick")));
    assert!(solo_keeps_source_audible(&runtime, index("bass")));
    assert!(!solo_keeps_source_audible(&runtime, index("keys")));
}

#[test]
fn track_input_state_updates_without_rebuilding_runtime() {
    let mut cache = HashMap::new();
    let snapshot = two_track_snapshot(48_000);
    let mut runtime =
        RuntimeProject::build(&snapshot, 48_000, &mut cache, None, true).expect("build");
    let original_track_count = runtime.tracks.len();

    runtime.update_track_input_state(
        0,
        true,
        true,
        RuntimeTrackInputSource::Stereo { left: 2, right: 3 },
    );

    let track = &runtime.tracks[0];
    assert!(track.record_armed);
    assert!(track.monitor_enabled);
    assert_eq!(
        track.input_source,
        RuntimeTrackInputSource::Stereo { left: 2, right: 3 }
    );
    assert_eq!(runtime.tracks.len(), original_track_count);
}

/// `reset_pdc_delay_lines` must zero every track's delay ring and rewind the
/// write cursor, so a transport (re)start/seek never replays stale audio that
/// would desync the compensated tracks from plugin/VSTi-latency tracks. This
/// is the realtime equivalent of export building a fresh, zeroed runtime.
#[test]
fn reset_pdc_delay_lines_clears_stale_audio() {
    let mut cache = HashMap::new();
    let snapshot = two_track_snapshot(48_000);
    let mut runtime =
        RuntimeProject::build(&snapshot, 48_000, &mut cache, None, true).expect("build");

    // Simulate residual delay-line audio + a drifted write cursor as if a
    // prior playback/seek left state behind.
    for track in &mut runtime.tracks {
        assert!(
            !track.pdc_delay_l.is_empty(),
            "delay lines are sized at build"
        );
        track.pdc_delay_l.fill(0.42);
        track.pdc_delay_r.fill(-0.42);
        track.pdc_write_pos = 7;
    }

    runtime.reset_pdc_delay_lines();

    for track in &runtime.tracks {
        assert!(track.pdc_delay_l.iter().all(|&s| s == 0.0));
        assert!(track.pdc_delay_r.iter().all(|&s| s == 0.0));
        assert_eq!(track.pdc_write_pos, 0);
    }
}

/// The runtime must time everything off the *active* opened-stream rate, not
/// the requested/project rate carried in the snapshot. Reproduces the
/// reported 48 kHz-requested / 96 kHz-active divergence and pins the spec
/// numbers: at 128 BPM a beat is exactly 45000 samples @ 96 kHz.
#[test]
fn runtime_uses_active_rate_not_requested_for_beat_math() {
    let mut cache = HashMap::new();
    // Snapshot carries the *requested* project rate (48 kHz)…
    let mut snapshot = two_track_snapshot(48_000);
    snapshot.bpm = 128.0;
    // …but the device opened at 96 kHz (active rate) — that is what build gets.
    let active_rate = 96_000;
    let runtime =
        RuntimeProject::build(&snapshot, active_rate, &mut cache, None, true).expect("build");

    assert_eq!(
        runtime.sample_rate, active_rate,
        "runtime must adopt the active opened-stream rate, not the snapshot's requested rate"
    );

    // PPQ conversion uses the active rate: one beat @ 128 BPM @ 96 kHz = 45000.
    let tempo_map = TempoMap::static_tempo(snapshot.bpm);
    let samples_per_beat = tempo_map.samples_at_beat(1.0, runtime.sample_rate as f64);
    assert_eq!(samples_per_beat, 45_000);
    // …and NOT the 22500 the requested 48 kHz rate would have produced.
    assert_eq!(tempo_map.samples_at_beat(1.0, 48_000.0), 22_500);
    assert_ne!(samples_per_beat, tempo_map.samples_at_beat(1.0, 48_000.0));
}

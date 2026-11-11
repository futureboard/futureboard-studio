
use super::*;
use crate::audio_file::AudioFileBuffer;
use crate::types::{EngineFadeSnapshot, EngineMidiClipSnapshot, EngineMidiNoteSnapshot};

fn test_source(frames: u64) -> Arc<ClipAudioSource> {
    Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: frames as usize,
        samples: vec![0.0; frames as usize * 2],
    })))
}

fn test_clip(stretch: StretchParams) -> EngineClipSnapshot {
    EngineClipSnapshot {
        id: "clip".to_string(),
        track_id: "track".to_string(),
        asset_id: "asset".to_string(),
        media_path: Some("test.wav".to_string()),
        start_beat: 0.0,
        duration_beats: 1.0,
        offset_seconds: 0.0,
        gain: 1.0,
        muted: false,
        ara_rendered: false,
        fades: Some(EngineFadeSnapshot {
            in_duration: 0.0,
            out_duration: 0.0,
            in_curve: "linear".to_string(),
            out_curve: "linear".to_string(),
        }),
        stretch,
        audio_process: None,
    }
}

#[test]
fn engine_clip_stretch_serializes_roundtrip() {
    let stretch = StretchParams {
        mode: StretchMode::Manual,
        algorithm: StretchAlgorithm::PreservePitch,
        time_ratio: 2.0,
        pitch_ratio: 1.25,
        preserve_pitch: true,
        ..StretchParams::default()
    };
    let clip = test_clip(stretch.clone());
    let json = serde_json::to_string(&clip).expect("serialize clip");
    let loaded: EngineClipSnapshot = serde_json::from_str(&json).expect("deserialize clip");
    assert_eq!(loaded.stretch, stretch);
}

#[test]
fn missing_stretch_defaults_to_off() {
    let json = r#"{
            "id":"clip","trackId":"track","assetId":"asset","mediaPath":"test.wav",
            "startBeat":0.0,"durationBeats":1.0,"offsetSeconds":0.0,"gain":1.0
        }"#;
    let loaded: EngineClipSnapshot = serde_json::from_str(json).expect("deserialize legacy clip");
    assert_eq!(loaded.stretch, StretchParams::default());
    assert_eq!(
        resolved_clip_stretch_params(&loaded),
        StretchParams::default()
    );
}

#[test]
fn legacy_audio_process_migrates_to_stretch_params() {
    let mut clip = test_clip(StretchParams::default());
    clip.audio_process = Some(EngineClipAudioProcess {
        speed_ratio: 0.5,
        effective_time_ratio: 2.0,
        pitch_ratio: 1.0,
        pitch_semitones: 0.0,
        preserve_pitch: true,
        mode: "manual".to_string(),
        quality: "balanced".to_string(),
        source_start_samples: 0,
        source_end_samples: 48_000,
        warp_markers: Vec::new(),
        reverse: false,
        denoise_amount: 0.0,
        channel_transform: 0,
        dc_remove: false,
        dc_left: 0.0,
        dc_right: 0.0,
        extra_gain: 1.0,
        dehum_hz: 0.0,
        dehum_harmonics: 0,
        dehum_reduction_db: 0.0,
        envelope_points: Vec::new(),
        preview_bypass: false,
    });
    let migrated = resolved_clip_stretch_params(&clip);
    assert_eq!(migrated.mode, StretchMode::Manual);
    assert_eq!(migrated.algorithm, StretchAlgorithm::PreservePitch);
    assert!(migrated.preserve_pitch);
    assert!((migrated.time_ratio - 2.0).abs() < f32::EPSILON);
}

#[test]
fn midi_and_audio_ppq_offsets_match_across_sample_rates() {
    let bpm = 128.0;
    let beats_per_second = bpm / 60.0;
    let tempo_map = TempoMap::static_tempo(bpm).snapshot();
    for sample_rate in [44_100, 48_000, 88_200, 96_000, 192_000] {
        let audio_at_zero = {
            let mut clip = test_clip(StretchParams::default());
            clip.start_beat = 0.0;
            build_clip_runtime(
                &clip,
                test_source(sample_rate as u64),
                beats_per_second,
                sample_rate,
                None,
            )
            .expect("audio clip at ppq 0")
        };
        assert_eq!(audio_at_zero.start_sample, 0);

        let audio_at_one = {
            let mut clip = test_clip(StretchParams::default());
            clip.start_beat = 1.0;
            build_clip_runtime(
                &clip,
                test_source(sample_rate as u64),
                beats_per_second,
                sample_rate,
                None,
            )
            .expect("audio clip at ppq 1")
        };

        let midi_clip = EngineMidiClipSnapshot {
            source_track_id: None,
            id: "midi".to_string(),
            track_id: "track".to_string(),
            start_beat: 0.0,
            length_beats: 2.0,
            notes: vec![
                EngineMidiNoteSnapshot {
                    id: 1,
                    pitch: 60,
                    start_beat: 0.0,
                    length_beats: 0.25,
                    velocity: 100,
                    channel: 0,
                    expression: sphere_midi_service::NoteExpression::default(),
                    pitch_points: Vec::new(),
                    articulation: None,
                },
                EngineMidiNoteSnapshot {
                    id: 2,
                    pitch: 61,
                    start_beat: 1.0,
                    length_beats: 0.25,
                    velocity: 100,
                    channel: 0,
                    expression: sphere_midi_service::NoteExpression::default(),
                    pitch_points: Vec::new(),
                    articulation: None,
                },
            ],
            controllers: Vec::new(),
            mpe: sphere_midi_service::mpe::MpeTrackConfiguration::default(),
        };
        let (_clips, tracks) =
            build_midi_runtime(&[std::sync::Arc::new(midi_clip)], &tempo_map, sample_rate);
        let note_on_samples: Vec<u64> = tracks[0]
            .events
            .iter()
            .filter(|event| matches!(event.kind, RuntimeMidiEventKind::NoteOn))
            .map(|event| event.sample)
            .collect();

        let expected_ppq_1 = tempo_map.samples_at_beat(1.0, sample_rate as f64);
        assert_eq!(note_on_samples[0], 0, "sr={sample_rate}");
        assert_eq!(note_on_samples[1], expected_ppq_1, "sr={sample_rate}");
        assert_eq!(
            audio_at_one.start_sample, expected_ppq_1,
            "sr={sample_rate}"
        );
    }
}

#[test]
fn build_runtime_uses_stretched_duration_samples() {
    let stretch = StretchParams {
        mode: StretchMode::Manual,
        algorithm: StretchAlgorithm::RePitch,
        time_ratio: 2.0,
        preserve_pitch: false,
        ..StretchParams::default()
    };
    let clip = test_clip(stretch.clone());
    let runtime_clip =
        build_clip_runtime(&clip, test_source(48_000), 2.0, 48_000, None).expect("runtime clip");
    assert_eq!(runtime_clip.duration_samples, 96_000);
    assert_eq!(runtime_clip.stretch, stretch);
    assert!((runtime_clip.source_read_rate - 0.5).abs() < f32::EPSILON);

    let stretch = StretchParams {
        time_ratio: 0.5,
        ..stretch
    };
    let clip = test_clip(stretch);
    let runtime_clip =
        build_clip_runtime(&clip, test_source(48_000), 2.0, 48_000, None).expect("runtime clip");
    assert_eq!(runtime_clip.duration_samples, 24_000);
    assert!((runtime_clip.source_read_rate - 2.0).abs() < f32::EPSILON);
}

/// 120 BPM for four beats, then 60.
fn halving_tempo_map() -> RuntimeTempoMapSnapshot {
    TempoMap::from_points(
        120.0,
        vec![TempoPoint::hold(0.0, 120.0), TempoPoint::hold(4.0, 60.0)],
    )
    .snapshot()
}

#[test]
fn only_a_single_tempo_equal_to_the_base_counts_as_flat() {
    assert!(tempo_map_is_flat_at(
        &RuntimeTempoMapSnapshot::static_tempo(120.0),
        120.0
    ));
    assert!(!tempo_map_is_flat_at(
        &RuntimeTempoMapSnapshot::static_tempo(120.0),
        100.0
    ));
    assert!(!tempo_map_is_flat_at(&halving_tempo_map(), 120.0));
}

/// A Tempo Sync clip is defined in beats, so a tempo change inside it has
/// to change its speed there: the half after the drop to 60 BPM plays at
/// half speed and takes twice as long.
#[test]
fn a_tempo_synced_clip_follows_a_tempo_change_inside_it() {
    let map = halving_tempo_map();
    let mut clip = test_clip(StretchParams {
        mode: StretchMode::TempoSync,
        algorithm: StretchAlgorithm::RePitch,
        source_bpm: Some(120.0),
        target_bpm: Some(120.0),
        ..StretchParams::default()
    });
    clip.duration_beats = 8.0;
    // Eight beats of a 120 BPM source: four seconds.
    let runtime_clip = build_clip_runtime(&clip, test_source(192_000), 2.0, 48_000, Some(&map))
        .expect("runtime clip");
    // Four beats at 120 (two seconds), four at 60 (four seconds).
    assert_eq!(runtime_clip.duration_samples, 288_000);
    let source_at = |out: u64| {
        map_warp_source_frame(
            out,
            runtime_clip.duration_samples,
            false,
            &runtime_clip.warp_segments,
        )
        .expect("warp segments")
    };
    assert!(
        (source_at(48_000) - 48_000.0).abs() < 1.0,
        "beat 2 at full speed"
    );
    assert!(
        (source_at(96_000) - 96_000.0).abs() < 1.0,
        "beat 4 is half the source"
    );
    assert!(
        (source_at(192_000) - 144_000.0).abs() < 1.0,
        "beat 6 at half speed"
    );
}

/// The same clip on the flat base tempo keeps the one-ratio timing, with
/// no segments at all.
#[test]
fn a_tempo_synced_clip_on_a_flat_tempo_needs_no_segments() {
    let mut clip = test_clip(StretchParams {
        mode: StretchMode::TempoSync,
        algorithm: StretchAlgorithm::RePitch,
        source_bpm: Some(120.0),
        target_bpm: Some(120.0),
        ..StretchParams::default()
    });
    clip.duration_beats = 8.0;
    let runtime_clip =
        build_clip_runtime(&clip, test_source(192_000), 2.0, 48_000, None).expect("runtime clip");
    assert_eq!(runtime_clip.duration_samples, 192_000);
    assert!(runtime_clip.warp_segments.is_empty());
}

/// Every clip is placed where the tempo map puts its beat, the one MIDI
/// and the metronome play from; one not locked to the tempo keeps its
/// wall-clock length.
#[test]
fn a_clip_after_a_tempo_change_starts_where_the_map_puts_it() {
    let map = halving_tempo_map();
    let mut clip = test_clip(StretchParams::default());
    clip.start_beat = 8.0;
    let placed = build_clip_runtime(&clip, test_source(48_000), 2.0, 48_000, Some(&map))
        .expect("runtime clip");
    // Two seconds for beats 0..4, four for beats 4..8.
    assert_eq!(placed.start_sample, 288_000);
    let flat =
        build_clip_runtime(&clip, test_source(48_000), 2.0, 48_000, None).expect("runtime clip");
    assert_eq!(placed.duration_samples, flat.duration_samples);
    assert!(placed.warp_segments.is_empty());
}

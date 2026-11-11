use super::*;
use crate::streaming_source::StreamingSource;

/// A short stereo 16-bit WAV; only its existence matters here.
fn write_wav(path: &std::path::Path) {
    let frames = 4_800u32;
    let data_len = frames * 4;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.resize(44 + data_len as usize, 0);
    std::fs::write(path, bytes).expect("write test wav");
}

fn open_stream(path: &str) -> Result<ClipAudioSource, String> {
    StreamingSource::open(path).map(|source| ClipAudioSource::Streaming(Arc::new(source)))
}

fn clip(
    id: &str,
    path: &str,
    start_beat: f64,
    duration_beats: f64,
    source: &Arc<ClipAudioSource>,
) -> RuntimeClip {
    let snapshot = EngineClipSnapshot {
        id: id.to_string(),
        track_id: "track".to_string(),
        asset_id: "asset".to_string(),
        media_path: Some(path.to_string()),
        start_beat,
        duration_beats,
        offset_seconds: 0.0,
        gain: 1.0,
        muted: false,
        ara_rendered: false,
        fades: None,
        stretch: StretchParams::default(),
        audio_process: None,
    };
    build_clip_runtime(&snapshot, Arc::clone(source), 2.0, 48_000, None).expect("runtime clip")
}

/// #188: clips of one disk-streamed file that sound at the same time must
/// not share a stream (one read cursor); clips that follow each other can.
#[test]
fn overlapping_clips_of_one_streamed_file_get_their_own_stream() {
    let file = std::env::temp_dir().join(format!("fb_stream_lanes_{}.wav", std::process::id()));
    write_wav(&file);
    let path = file.to_str().expect("utf-8 temp path").to_string();

    let shared = Arc::new(open_stream(&path).expect("open stream"));
    let mut cache = HashMap::new();
    cache.insert(path.clone(), Arc::clone(&shared));
    // a: beats 0..4, b: 2..6 (overlaps a), c: 4..8 (starts as a ends).
    let layout = |shared: &Arc<ClipAudioSource>| {
        vec![
            clip("a", &path, 0.0, 4.0, shared),
            clip("b", &path, 2.0, 4.0, shared),
            clip("c", &path, 4.0, 4.0, shared),
        ]
    };
    let paths = vec![path.as_str(); 3];

    let mut clips = layout(&shared);
    let opened = assign_stream_lanes(&mut clips, &paths, &mut cache, &mut open_stream);
    assert_eq!(opened, 1, "two clips overlap at most, so one extra stream");
    assert!(Arc::ptr_eq(&clips[0].source, &shared));
    assert!(
        !Arc::ptr_eq(&clips[1].source, &shared),
        "b plays while a does and must have its own stream"
    );
    assert!(clips[1].source.is_streaming());
    assert!(
        Arc::ptr_eq(&clips[2].source, &shared),
        "c starts when a ends and reuses its stream"
    );

    // A rebuild reuses the open lane instead of opening the file again.
    let lane = Arc::clone(&clips[1].source);
    let mut rebuilt = layout(&shared);
    assert_eq!(
        assign_stream_lanes(&mut rebuilt, &paths, &mut cache, &mut open_stream),
        0
    );
    assert!(Arc::ptr_eq(&rebuilt[1].source, &lane));

    // With nothing overlapping any more the extra stream is released.
    let mut alone = vec![clip("a", &path, 0.0, 4.0, &shared)];
    assign_stream_lanes(&mut alone, &paths[..1], &mut cache, &mut open_stream);
    assert_eq!(cache.len(), 1);

    drop((clips, rebuilt, alone, lane, shared, cache));
    let _ = std::fs::remove_file(&file);
}

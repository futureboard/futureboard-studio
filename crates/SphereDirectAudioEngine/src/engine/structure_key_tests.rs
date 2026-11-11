use super::{project_structure_key, write_canonical_json};
use crate::types::{EngineMidiClipSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot};

fn snapshot(bpm: f64, midi_clip_ids: &[&str]) -> EngineProjectSnapshot {
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "structure-key".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks: Vec::new(),
        clips: Vec::new(),
        midi_clips: midi_clip_ids
            .iter()
            .map(|id| {
                std::sync::Arc::new(EngineMidiClipSnapshot {
                    source_track_id: None,
                    id: id.to_string(),
                    track_id: "track-1".to_string(),
                    start_beat: 0.0,
                    length_beats: 4.0,
                    notes: Vec::new(),
                    controllers: Vec::new(),
                    mpe: Default::default(),
                })
            })
            .collect(),
        pdc_enabled: true,
        latency_graph_version: 0,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: 48_000,
            buffer_size: 256,
        },
    }
}

/// A note edit keeps the key, so it takes the MIDI-only path; anything
/// else about the project, or the graph's build settings, changes it.
#[test]
fn only_the_midi_is_left_out_of_the_key() {
    let mut a = snapshot(120.0, &["clip-1"]);
    let mut b = snapshot(120.0, &["clip-1", "clip-2"]);
    let key = project_structure_key(&mut a, 48_000, true);
    assert!(key.is_some());
    assert_eq!(key, project_structure_key(&mut b, 48_000, true));
    assert_eq!(b.midi_clips.len(), 2, "the MIDI is put back");

    let mut faster = snapshot(140.0, &["clip-1"]);
    assert_ne!(key, project_structure_key(&mut faster, 48_000, true));
    assert_ne!(key, project_structure_key(&mut a, 44_100, true));
    assert_ne!(key, project_structure_key(&mut a, 48_000, false));
}

fn soundfont_track(mode: &str) -> crate::types::EngineTrackSnapshot {
    serde_json::from_value(serde_json::json!({
        "id": "sf", "type": "instrument", "volume": 1.0, "pan": 0.0,
        "muted": false, "solo": false, "armed": false,
        "outputTrackId": null, "inserts": [],
        "builtinSoundfontPlayer": true, "soundfontPath": "font.sf2",
        "soundfontMode": mode,
    }))
    .expect("track snapshot")
}

/// A multitimbral player's parts are applied live, so moving one leaves
/// the key alone; switching the player's mode rebuilds it.
#[test]
fn soundfont_parts_are_left_out_of_the_key_and_the_mode_is_not() {
    let mut a = snapshot(120.0, &[]);
    a.tracks.push(soundfont_track("Multi"));
    let key = project_structure_key(&mut a, 48_000, true);

    let mut parts = a.clone();
    parts.tracks[0].soundfont_channels[3].volume = 40;
    parts.tracks[0].soundfont_channels[9].preset = Some((128, 0));
    assert_eq!(key, project_structure_key(&mut parts, 48_000, true));
    assert_eq!(
        parts.tracks[0].soundfont_channels[3].volume, 40,
        "the parts are put back"
    );

    let mut single = snapshot(120.0, &[]);
    single.tracks.push(soundfont_track("Single"));
    assert_ne!(key, project_structure_key(&mut single, 48_000, true));
}

/// Clip gain is applied live (`SetClipGain`), so a gain change leaves the
/// key alone and the gains are put back; anything else about the clip
/// still changes it.
#[test]
fn clip_gain_is_left_out_of_the_key() {
    let clip = |gain: f32, start_beat: f64| crate::types::EngineClipSnapshot {
        id: "clip-1".to_string(),
        track_id: "audio".to_string(),
        asset_id: "asset-1".to_string(),
        media_path: Some("take.wav".to_string()),
        start_beat,
        duration_beats: 4.0,
        offset_seconds: 0.0,
        gain,
        muted: false,
        ara_rendered: false,
        fades: None,
        stretch: Default::default(),
        audio_process: None,
    };
    let mut a = snapshot(120.0, &[]);
    a.clips.push(clip(1.0, 0.0));
    let key = project_structure_key(&mut a, 48_000, true);

    let mut louder = snapshot(120.0, &[]);
    louder.clips.push(clip(1.4, 0.0));
    assert_eq!(key, project_structure_key(&mut louder, 48_000, true));
    assert_eq!(louder.clips[0].gain, 1.4, "the gain is put back");

    let mut moved = snapshot(120.0, &[]);
    moved.clips.push(clip(1.0, 2.0));
    assert_ne!(key, project_structure_key(&mut moved, 48_000, true));
}

/// Insert params are a `HashMap`: the same entries in another order must
/// give the same key.
#[test]
fn object_key_order_does_not_change_the_key() {
    let one: serde_json::Value =
        serde_json::from_str(r#"{"b":1,"a":{"y":2,"x":[3,{"q":4,"p":5}]}}"#).unwrap();
    let two: serde_json::Value =
        serde_json::from_str(r#"{"a":{"x":[3,{"p":5,"q":4}],"y":2},"b":1}"#).unwrap();
    let (mut left, mut right) = (String::new(), String::new());
    write_canonical_json(&one, &mut left);
    write_canonical_json(&two, &mut right);
    assert_eq!(left, right);
    assert_eq!(left, r#"{"a":{"x":[3,{"p":5,"q":4}],"y":2},"b":1}"#);
}

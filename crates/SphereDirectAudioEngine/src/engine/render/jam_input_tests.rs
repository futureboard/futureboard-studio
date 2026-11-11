use super::render_project_block_interleaved_with_inputs;
use crate::jam_bus::{jam_device_id, JamAudioBus};
use crate::runtime::{RuntimeProject, RuntimeTrackInputSource};
use crate::types::{
    EngineProjectSnapshot, EngineRoutingSnapshot, EngineTrackInputSourceSnapshot,
    EngineTrackSnapshot,
};
use std::collections::HashMap;

const FRAMES: usize = 64;
const STREAM: &str = "str_guitar";

fn track(id: &str, track_type: &str, input: EngineTrackInputSourceSnapshot) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: track_type.to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: track_type == "audio",
        input_source: input,
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

fn snapshot(input: EngineTrackInputSourceSnapshot) -> EngineProjectSnapshot {
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "jam-test".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks: vec![
            track("audio-1", "audio", input),
            track("master", "master", Default::default()),
        ],
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: 48_000,
            buffer_size: 256,
        },
    }
}

fn jam_route() -> EngineTrackInputSourceSnapshot {
    EngineTrackInputSourceSnapshot {
        device_id: Some(jam_device_id(STREAM)),
        channels: vec![0, 1],
    }
}

fn build(snapshot: &EngineProjectSnapshot, bus: &JamAudioBus) -> RuntimeProject {
    let mut runtime = RuntimeProject::build(snapshot, 48_000, &mut HashMap::new(), None, true)
        .expect("jam runtime");
    runtime.resolve_jam_inputs(snapshot, bus);
    runtime.resolve_loopback_inputs(snapshot);
    runtime
}

fn render(
    runtime: &mut RuntimeProject,
    bus: &JamAudioBus,
    live_input: Option<(&[f32], &[f32])>,
) -> (f32, f32) {
    let mut output = [0.0f32; FRAMES * 2];
    render_project_block_interleaved_with_inputs(
        runtime,
        0,
        1.0,
        &mut output,
        2,
        false,
        4,
        4,
        None,
        live_input,
        Some(bus),
    );
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    for frame in output.chunks(2) {
        peak_l = peak_l.max(frame[0].abs());
        peak_r = peak_r.max(frame[1].abs());
    }
    (peak_l, peak_r)
}

fn remote_block(left: f32, right: f32) -> Vec<f32> {
    (0..FRAMES).flat_map(|_| [left, right]).collect()
}

/// The first milestone in one test: a remote stream lands in the bus and is
/// audible on a track routed to it.
#[test]
fn a_remote_stream_is_heard_on_the_track_routed_to_it() {
    let bus = JamAudioBus::new();
    let snapshot = snapshot(jam_route());
    let mut runtime = build(&snapshot, &bus);

    let slot_index = bus
        .input_slot_for(STREAM)
        .expect("the route claimed a slot");
    assert!(matches!(
        runtime.tracks[0].input_source,
        RuntimeTrackInputSource::Jam { .. }
    ));

    // Silence first: nothing has arrived from the network yet.
    assert_eq!(render(&mut runtime, &bus, None), (0.0, 0.0));

    // One block of remote audio, captured at a session tick of its own.
    bus.input(slot_index).expect("in range").write_interleaved(
        &remote_block(0.5, -0.25),
        2,
        20_000_000,
        48_000,
    );
    bus.input(slot_index).expect("in range").assume_primed();

    let (peak_l, peak_r) = render(&mut runtime, &bus, None);
    assert!((peak_l - 0.5).abs() < 1e-6, "left was {peak_l}");
    assert!((peak_r - 0.25).abs() < 1e-6, "right was {peak_r}");
}

/// A jam track must not also pick up whatever is plugged into the audio
/// interface: they are two different performers.
#[test]
fn a_jam_track_does_not_double_with_the_hardware_input() {
    let bus = JamAudioBus::new();
    let snapshot = snapshot(jam_route());
    let mut runtime = build(&snapshot, &bus);
    let slot_index = bus.input_slot_for(STREAM).expect("bound");

    bus.input(slot_index).expect("in range").write_interleaved(
        &remote_block(0.5, 0.5),
        2,
        0,
        48_000,
    );
    bus.input(slot_index).expect("in range").assume_primed();

    let hardware = [0.5f32; FRAMES];
    let (peak_l, _) = render(&mut runtime, &bus, Some((&hardware, &hardware)));
    assert!(
        (peak_l - 0.5).abs() < 1e-6,
        "the hardware input summed into the jam track: {peak_l}"
    );
}

/// The inverse: a hardware-routed track is untouched by a jam that happens
/// to be running.
#[test]
fn a_hardware_track_is_unaffected_by_a_live_jam() {
    let bus = JamAudioBus::new();
    let snapshot = snapshot(EngineTrackInputSourceSnapshot {
        device_id: Some("asio:test".to_string()),
        channels: vec![0, 1],
    });
    let mut runtime = build(&snapshot, &bus);
    assert!(!runtime.tracks[0].input_source.is_jam());

    // Another participant's stream is arriving, bound to a slot this track
    // does not reference.
    let slot_index = bus.bind_input("str_other").expect("bound");
    bus.input(slot_index).expect("in range").write_interleaved(
        &remote_block(1.0, 1.0),
        2,
        0,
        48_000,
    );
    bus.input(slot_index).expect("in range").assume_primed();

    let hardware = [0.25f32; FRAMES];
    let (peak_l, _) = render(&mut runtime, &bus, Some((&hardware, &hardware)));
    assert!((peak_l - 0.25).abs() < 1e-6, "left was {peak_l}");
}

/// A performer leaving mid-session must not leave the callback reading a
/// slot that has been handed to somebody else.
#[test]
fn a_released_stream_goes_silent_rather_than_playing_the_next_tenant() {
    let bus = JamAudioBus::new();
    let snapshot = snapshot(jam_route());
    let mut runtime = build(&snapshot, &bus);
    let slot_index = bus.input_slot_for(STREAM).expect("bound");

    bus.input(slot_index).expect("in range").write_interleaved(
        &remote_block(1.0, 1.0),
        2,
        0,
        48_000,
    );
    bus.input(slot_index).expect("in range").assume_primed();
    assert!(render(&mut runtime, &bus, None).0 > 0.5);

    bus.release_input(STREAM);
    // The runtime snapshot still points at the old index for one block.
    assert_eq!(render(&mut runtime, &bus, None), (0.0, 0.0));
}

/// A jam route with no slot left captures nothing rather than being pointed
/// at an unrelated stream.
#[test]
fn a_route_that_cannot_be_bound_resolves_to_no_input() {
    let bus = JamAudioBus::new();
    for index in 0..crate::jam_bus::MAX_JAM_INPUT_SLOTS {
        bus.bind_input(&format!("filler_{index}"));
    }
    let snapshot = snapshot(jam_route());
    let runtime = build(&snapshot, &bus);
    assert_eq!(
        runtime.tracks[0].input_source,
        RuntimeTrackInputSource::None
    );
}

/// The publish half: a track's post-fader block reaches the jam bus slot it
/// was bound to, and only that one.
#[test]
fn a_shared_track_writes_its_post_fader_block_to_its_publish_slot() {
    let bus = JamAudioBus::new();
    let snapshot = snapshot(jam_route());
    let mut runtime = build(&snapshot, &bus);
    let input_slot = bus.input_slot_for(STREAM).expect("bound");

    // Feed the track from the jam input so it has something post-fader.
    bus.input(input_slot).expect("in range").write_interleaved(
        &remote_block(0.5, 0.5),
        2,
        0,
        48_000,
    );
    bus.input(input_slot).expect("in range").assume_primed();

    // Nothing bound yet: the tap must not run.
    render(&mut runtime, &bus, None);
    assert!(bus.publish_slot_for("track:audio-1").is_none());

    let publish_slot = bus.bind_publish("track:audio-1").expect("a slot is free");
    runtime.update_track_jam_publish(0, Some(publish_slot as u32));
    bus.input(input_slot).expect("in range").write_interleaved(
        &remote_block(0.5, 0.5),
        2,
        0,
        48_000,
    );
    bus.input(input_slot).expect("in range").assume_primed();
    render(&mut runtime, &bus, None);

    let mut out = Vec::new();
    let (frames, _channels, _) = bus
        .publish(publish_slot)
        .expect("in range")
        .read_interleaved(&mut out, FRAMES)
        .expect("the tap wrote frames");
    assert_eq!(frames, FRAMES);
    let peak = out
        .iter()
        .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
    assert!((peak - 0.5).abs() < 1e-6, "the tap captured {peak}");
}

/// The multitrack half: every shared track lands in its own channel pair of
/// one stream, and the whole stream advances on a single head — which is
/// what makes the arriving take alignable as one recording.
#[test]
fn a_multitrack_stream_carries_each_shared_track_in_its_own_pair() {
    let bus = JamAudioBus::new();
    let snapshot = snapshot(jam_route());
    let mut runtime = build(&snapshot, &bus);
    let input_slot = bus.input_slot_for(STREAM).expect("bound");

    bus.bind_multitrack_publish(4)
        .expect("the wide slot is free");
    // Only one track exists in this fixture, so it takes pair 0 and pair 1
    // is left for a track that is not sharing.
    runtime.update_jam_multitrack_pairs(&[0]);

    bus.input(input_slot).expect("in range").write_interleaved(
        &remote_block(0.5, 0.5),
        2,
        0,
        48_000,
    );
    bus.input(input_slot).expect("in range").assume_primed();
    render(&mut runtime, &bus, None);

    let mut out = Vec::new();
    let (frames, channels, _) = bus
        .multitrack_publish()
        .expect("claimed")
        .read_interleaved(&mut out, FRAMES)
        .expect("the tap wrote frames");
    assert_eq!((frames, channels), (FRAMES, 4));

    let peak_first = out
        .chunks(channels)
        .fold(0.0f32, |peak, frame| peak.max(frame[0].abs()));
    let peak_second = out
        .chunks(channels)
        .fold(0.0f32, |peak, frame| peak.max(frame[2].abs()));
    assert!(
        (peak_first - 0.5).abs() < 1e-6,
        "pair 0 captured {peak_first}"
    );
    assert_eq!(
        peak_second, 0.0,
        "an unassigned pair is silence, not the neighbouring track"
    );
}

/// Clearing the assignment stops the tap, the same way unsharing one track
/// does. A stream that kept advancing would fill a ring nobody drains.
#[test]
fn clearing_the_multitrack_assignment_stops_the_stream() {
    let bus = JamAudioBus::new();
    let snapshot = snapshot(jam_route());
    let mut runtime = build(&snapshot, &bus);
    let input_slot = bus.input_slot_for(STREAM).expect("bound");
    bus.bind_multitrack_publish(2).expect("free");
    runtime.update_jam_multitrack_pairs(&[0]);

    bus.input(input_slot).expect("in range").write_interleaved(
        &remote_block(0.5, 0.5),
        2,
        0,
        48_000,
    );
    bus.input(input_slot).expect("in range").assume_primed();
    render(&mut runtime, &bus, None);
    let mut out = Vec::new();
    assert!(bus
        .multitrack_publish()
        .expect("claimed")
        .read_interleaved(&mut out, FRAMES)
        .is_some());

    bus.release_publish(crate::jam_bus::PUBLISH_KEY_MULTITRACK);
    runtime.update_jam_multitrack_pairs(&[]);
    render(&mut runtime, &bus, None);
    assert!(
        bus.multitrack_publish().is_none(),
        "the released slot is not reachable, so nothing can be written to it"
    );
}

/// Unsharing stops the tap. A slot that keeps filling after a publish ends
/// would hold audio nobody drains until the ring wrapped.
#[test]
fn unsharing_a_track_stops_the_tap() {
    let bus = JamAudioBus::new();
    let snapshot = snapshot(jam_route());
    let mut runtime = build(&snapshot, &bus);
    let input_slot = bus.input_slot_for(STREAM).expect("bound");
    let publish_slot = bus.bind_publish("track:audio-1").expect("a slot is free");
    runtime.update_track_jam_publish(0, Some(publish_slot as u32));

    bus.input(input_slot).expect("in range").write_interleaved(
        &remote_block(0.5, 0.5),
        2,
        0,
        48_000,
    );
    bus.input(input_slot).expect("in range").assume_primed();
    render(&mut runtime, &bus, None);
    let mut out = Vec::new();
    assert!(bus
        .publish(publish_slot)
        .expect("in range")
        .read_interleaved(&mut out, FRAMES)
        .is_some());

    runtime.update_track_jam_publish(0, None);
    bus.input(input_slot).expect("in range").write_interleaved(
        &remote_block(0.5, 0.5),
        2,
        0,
        48_000,
    );
    bus.input(input_slot).expect("in range").assume_primed();
    render(&mut runtime, &bus, None);
    assert!(
        bus.publish(publish_slot)
            .expect("in range")
            .read_interleaved(&mut out, FRAMES)
            .is_none(),
        "the tap kept writing after the publish ended"
    );
}

/// An unmonitored track is silent whatever is arriving for it, exactly as a
/// hardware input would be.
#[test]
fn an_unmonitored_jam_track_stays_silent() {
    let bus = JamAudioBus::new();
    let mut snapshot = snapshot(jam_route());
    snapshot.tracks[0].input_monitor = false;
    let mut runtime = build(&snapshot, &bus);
    let slot_index = bus.input_slot_for(STREAM).expect("bound");

    bus.input(slot_index).expect("in range").write_interleaved(
        &remote_block(1.0, 1.0),
        2,
        0,
        48_000,
    );
    bus.input(slot_index).expect("in range").assume_primed();
    assert_eq!(render(&mut runtime, &bus, None), (0.0, 0.0));
}

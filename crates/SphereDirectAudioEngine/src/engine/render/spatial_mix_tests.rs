use super::{apply_fader, mix_spatial_bus, route_main_output};
use crate::export::{make_track_snapshot, silence_snapshot};
use crate::runtime::RuntimeProject;
use crate::types::{EngineSpatialSnapshot, EngineSpatialSource};
use solfege_spatialaudio::{
    MonitorFold, RoomPosition, RoomSettings, SourceParams, SpatialFormat, SpeakerLayout,
};

const FRAMES: usize = 256;

fn runtime(format: SpatialFormat, fold: MonitorFold, x: f32, y: f32) -> RuntimeProject {
    let mut snapshot = silence_snapshot(48_000);
    snapshot.tracks = vec![make_track_snapshot("t1")];
    snapshot.spatial = EngineSpatialSnapshot {
        format,
        room: RoomSettings {
            reflections: 0.0,
            ..RoomSettings::default()
        },
        fold,
        sources: vec![EngineSpatialSource {
            track_id: "t1".into(),
            params: SourceParams {
                position: RoomPosition::new(x, y, 0.0),
                width: 0.0,
                ..SourceParams::default()
            },
            keeps_inputs: false,
        }],
    };
    let mut cache = std::collections::HashMap::new();
    RuntimeProject::build(&snapshot, 48_000, &mut cache, None, true).expect("build")
}

/// Two blocks of a steady tone through the track into `channels` device
/// channels; the second block's interleaved output.
fn render(runtime: &mut RuntimeProject, channels: usize) -> (Vec<f32>, Option<usize>) {
    let mut out = vec![0.0f32; FRAMES * channels];
    let mut discrete = None;
    for _ in 0..2 {
        out.fill(0.0);
        runtime.spatial.bus.clear(FRAMES);
        let track = &mut runtime.tracks[0];
        for n in 0..FRAMES {
            let v = (n as f32 * 0.21).sin() * 0.5;
            track.block_l[n] = v;
            track.block_r[n] = v;
        }
        route_main_output(runtime, 0, FRAMES, &mut out, channels);
        discrete = mix_spatial_bus(runtime, FRAMES, &mut out, channels);
    }
    (out, discrete)
}

fn energy(out: &[f32], channels: usize, channel: usize) -> f32 {
    out.chunks(channels).map(|f| f[channel] * f[channel]).sum()
}

#[test]
fn a_stereo_mix_keeps_the_ordinary_sum() {
    let mut rt = runtime(SpatialFormat::Stereo, MonitorFold::Binaural, 1.0, 0.0);
    assert!(rt.tracks[0].spatial.is_none());
    let (out, discrete) = render(&mut rt, 2);
    assert_eq!(discrete, None);
    assert!((energy(&out, 2, 0) - energy(&out, 2, 1)).abs() < 1.0e-4);
}

#[test]
fn a_binaural_channel_on_the_right_is_heard_on_the_right() {
    let mut rt = runtime(SpatialFormat::Binaural, MonitorFold::Binaural, 1.0, 0.0);
    assert!(rt.tracks[0].spatial.is_some());
    let (out, discrete) = render(&mut rt, 2);
    assert_eq!(discrete, None);
    assert!(energy(&out, 2, 1) > energy(&out, 2, 0) * 1.5);
}

#[test]
fn a_surround_mix_goes_out_discretely_when_master_owns_the_device() {
    let mut rt = runtime(
        SpatialFormat::Surround(SpeakerLayout::Surround51),
        MonitorFold::Stereo,
        -1.0,
        -1.0,
    );
    rt.monitor.hardware_owner = crate::monitor::HardwareOutputOwner::MasterDirect;
    rt.monitor.master_output = Some((0, 1));
    let (out, discrete) = render(&mut rt, 8);
    assert_eq!(discrete, Some(6));
    // Left-rear corner: the left surround speaker, channel 4, carries it.
    let ls = energy(&out, 8, 4);
    assert!(ls > 1.0, "{ls}");
    for other in [0usize, 1, 2, 3] {
        assert!(energy(&out, 8, other) < ls * 0.05, "channel {other}");
    }
    // Nothing past the layout.
    assert_eq!(energy(&out, 8, 6) + energy(&out, 8, 7), 0.0);
}

#[test]
fn a_surround_mix_folds_to_the_first_pair_on_a_stereo_device() {
    // On the left wall: between L and Ls, both of which fold left.
    let mut rt = runtime(
        SpatialFormat::Surround(SpeakerLayout::Surround51),
        MonitorFold::Stereo,
        -1.0,
        0.0,
    );
    let (out, discrete) = render(&mut rt, 2);
    assert_eq!(discrete, None);
    assert!(energy(&out, 2, 0) > 1.0 && energy(&out, 2, 1) < energy(&out, 2, 0) * 0.05);
}

#[test]
fn a_live_move_reaches_the_channel() {
    let mut rt = runtime(SpatialFormat::Binaural, MonitorFold::Binaural, 1.0, 0.0);
    rt.update_track_spatial(
        0,
        SourceParams {
            position: RoomPosition::new(-1.0, 0.0, 0.0),
            width: 0.0,
            ..SourceParams::default()
        },
    );
    let (out, _) = render(&mut rt, 2);
    assert!(energy(&out, 2, 0) > energy(&out, 2, 1) * 1.5);
}

/// A lane that moves the channel: placed on the right, a Left/Right lane
/// (tag 9) holding it hard left wins while it reads.
#[test]
fn a_spatial_lane_places_the_channel_where_it_says() {
    use crate::types::{
        EngineAutomationLaneSnapshot, EngineAutomationPointSnapshot, EngineAutomationTargetSnapshot,
    };
    let mut snapshot = silence_snapshot(48_000);
    let mut track = make_track_snapshot("t1");
    let point = |beat: f64| EngineAutomationPointSnapshot {
        beat,
        value: 0.0,
        curve: 0,
        tension: 0.0,
    };
    track.automation_lanes = vec![EngineAutomationLaneSnapshot {
        id: "lane".into(),
        name: "Left/Right".into(),
        target: EngineAutomationTargetSnapshot {
            tag: 9,
            ..Default::default()
        },
        enabled: true,
        points: vec![point(0.0), point(64.0)],
    }];
    snapshot.tracks = vec![track];
    snapshot.spatial = EngineSpatialSnapshot {
        format: SpatialFormat::Binaural,
        room: RoomSettings {
            reflections: 0.0,
            ..RoomSettings::default()
        },
        fold: MonitorFold::Binaural,
        sources: vec![EngineSpatialSource {
            track_id: "t1".into(),
            params: SourceParams {
                position: RoomPosition::new(1.0, 0.0, 0.0),
                width: 0.0,
                ..SourceParams::default()
            },
            keeps_inputs: false,
        }],
    };
    let mut cache = std::collections::HashMap::new();
    let mut rt = RuntimeProject::build(&snapshot, 48_000, &mut cache, None, true).expect("build");
    let mut out = vec![0.0f32; FRAMES * 2];
    for _ in 0..2 {
        out.fill(0.0);
        rt.spatial.bus.clear(FRAMES);
        for n in 0..FRAMES {
            let v = (n as f32 * 0.21).sin() * 0.5;
            rt.tracks[0].block_l[n] = v;
            rt.tracks[0].block_r[n] = v;
        }
        apply_fader(&mut rt.tracks[0], FRAMES, 1.0, false);
        route_main_output(&mut rt, 0, FRAMES, &mut out, 2);
        mix_spatial_bus(&mut rt, FRAMES, &mut out, 2);
    }
    assert!(rt.tracks[0].spatial_state.heard.position.x < -0.99);
    assert!(energy(&out, 2, 0) > energy(&out, 2, 1) * 1.5);
}

/// A bus that keeps its channels' places renders a channel routed into it
/// where the channel sits, rides the field on its fader, and reaches the
/// master directly as far as compensation is concerned.
#[test]
fn a_bus_that_keeps_its_channels_places_them_at_its_fader() {
    let mut snapshot = silence_snapshot(48_000);
    let mut source = make_track_snapshot("t1");
    source.output_track_id = Some("bus".into());
    let mut bus = make_track_snapshot("bus");
    bus.track_type = "bus".into();
    bus.volume = 0.5;
    snapshot.tracks = vec![source, bus];
    snapshot.spatial = EngineSpatialSnapshot {
        format: SpatialFormat::Surround(SpeakerLayout::Surround51),
        room: RoomSettings::default(),
        fold: MonitorFold::Stereo,
        sources: vec![
            EngineSpatialSource {
                track_id: "t1".into(),
                params: SourceParams {
                    position: RoomPosition::new(-1.0, -1.0, 0.0),
                    width: 0.0,
                    ..SourceParams::default()
                },
                keeps_inputs: false,
            },
            EngineSpatialSource {
                track_id: "bus".into(),
                params: SourceParams::default(),
                keeps_inputs: true,
            },
        ],
    };
    let mut cache = std::collections::HashMap::new();
    let mut rt = RuntimeProject::build(&snapshot, 48_000, &mut cache, None, true).expect("build");
    rt.monitor.hardware_owner = crate::monitor::HardwareOutputOwner::MasterDirect;
    rt.monitor.master_output = Some((0, 1));
    assert_eq!(crate::runtime::spatial_field_target(&rt.tracks, 0), Some(1));
    assert!(rt.tracks[0].spatial.is_some(), "the channel is placed");
    assert!(
        rt.tracks[1].spatial_state.field.is_some(),
        "the bus has a field"
    );

    let channels = 8;
    let mut out = vec![0.0f32; FRAMES * channels];
    for _ in 0..2 {
        out.fill(0.0);
        rt.spatial.bus.clear(FRAMES);
        if let Some(field) = rt.tracks[1].spatial_state.field.as_mut() {
            field.clear(FRAMES);
        }
        for n in 0..FRAMES {
            let v = (n as f32 * 0.21).sin() * 0.5;
            rt.tracks[0].block_l[n] = v;
            rt.tracks[0].block_r[n] = v;
            rt.tracks[1].block_l[n] = 0.0;
            rt.tracks[1].block_r[n] = 0.0;
        }
        apply_fader(&mut rt.tracks[0], FRAMES, 1.0, false);
        route_main_output(&mut rt, 0, FRAMES, &mut out, channels);
        // Nothing reached the bus's stereo input.
        assert!(rt.tracks[1].recv_l[..FRAMES].iter().all(|s| *s == 0.0));
        apply_fader(&mut rt.tracks[1], FRAMES, 1.0, false);
        route_main_output(&mut rt, 1, FRAMES, &mut out, channels);
        mix_spatial_bus(&mut rt, FRAMES, &mut out, channels);
    }
    // Left-rear corner: the left surround speaker carries it, at the bus's
    // fader (half level, a quarter of the energy of the same channel direct).
    let ls = energy(&out, channels, 4);
    let mut direct = runtime(
        SpatialFormat::Surround(SpeakerLayout::Surround51),
        MonitorFold::Stereo,
        -1.0,
        -1.0,
    );
    direct.monitor.hardware_owner = crate::monitor::HardwareOutputOwner::MasterDirect;
    direct.monitor.master_output = Some((0, 1));
    let (direct_out, _) = render(&mut direct, channels);
    let direct_ls = energy(&direct_out, channels, 4);
    assert!(
        (ls / direct_ls - 0.25).abs() < 0.02,
        "bus field {ls} vs direct {direct_ls}"
    );
}

/// Without the flag the bus is placed as one source and the channel is not.
#[test]
fn an_ordinary_bus_is_placed_as_one_source() {
    let mut snapshot = silence_snapshot(48_000);
    let mut source = make_track_snapshot("t1");
    source.output_track_id = Some("bus".into());
    let mut bus = make_track_snapshot("bus");
    bus.track_type = "bus".into();
    snapshot.tracks = vec![source, bus];
    snapshot.spatial = EngineSpatialSnapshot {
        format: SpatialFormat::Surround(SpeakerLayout::Surround51),
        ..EngineSpatialSnapshot::default()
    };
    let mut cache = std::collections::HashMap::new();
    let rt = RuntimeProject::build(&snapshot, 48_000, &mut cache, None, true).expect("build");
    assert_eq!(crate::runtime::spatial_field_target(&rt.tracks, 0), None);
    assert!(rt.tracks[0].spatial.is_none());
    assert!(rt.tracks[1].spatial.is_some());
    assert!(rt.tracks[1].spatial_state.field.is_none());
}

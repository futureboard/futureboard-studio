use super::{mix_spatial_bus, route_main_output};
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

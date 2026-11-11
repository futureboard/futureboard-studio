use super::RuntimeTrackInputSource;

#[test]
fn mono_latest_sample_is_route_local_and_duplicated() {
    let source = RuntimeTrackInputSource::Mono { channel: 7 };

    assert_eq!(source.sample_from_latest(0.25, -0.75), (0.25, 0.25));
}

#[test]
fn stereo_latest_samples_are_route_local() {
    let source = RuntimeTrackInputSource::Stereo { left: 7, right: 2 };

    assert_eq!(source.sample_from_latest(0.25, -0.75), (0.25, -0.75));
}

#[test]
fn meter_samples_follow_the_published_hardware_pair() {
    let mono_right = RuntimeTrackInputSource::Mono { channel: 3 };
    assert_eq!(
        mono_right.sample_from_monitor_pair(0.25, -0.75, (2, 3)),
        (-0.75, -0.75)
    );

    let unavailable = RuntimeTrackInputSource::Mono { channel: 7 };
    assert_eq!(
        unavailable.sample_from_monitor_pair(0.25, -0.75, (2, 3)),
        (0.0, 0.0)
    );
}

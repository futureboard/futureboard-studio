use super::{monitor_channel_pair, routed_input_peaks, EngineInner, SharedState};
use std::sync::atomic::Ordering;

#[test]
fn mono_monitor_route_uses_one_channel_for_both_sides() {
    assert_eq!(monitor_channel_pair(&[5]), Some((5, 5)));
}

#[test]
fn mono_input_meter_duplicates_selected_channel_peak() {
    assert_eq!(routed_input_peaks(&[0.1, 0.4, 0.2], &[1]), Some((0.4, 0.4)));
}

#[test]
fn monitor_source_pair_publishes_both_channels_together() {
    let shared = SharedState::default();
    shared.set_monitor_source_pair(7, 3);
    assert_eq!(shared.monitor_source_pair(), (7, 3));
}

#[test]
fn monitor_shared_clock_defaults_false() {
    assert!(!SharedState::default()
        .monitor_shared_clock
        .load(Ordering::Relaxed));
}

#[test]
fn stopping_live_input_clears_shared_clock() {
    let engine = EngineInner::new();
    engine
        .shared
        .monitor_shared_clock
        .store(true, Ordering::Relaxed);

    engine.stop_live_input_stream();

    assert!(!engine.shared.monitor_shared_clock.load(Ordering::Relaxed));
}

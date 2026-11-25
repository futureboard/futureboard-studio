//! Dual pan in the fader stage: the 2x2 matrix, its per-block ramp into and
//! out of Dual, and the balance path left exactly as it was.

use super::*;
use crate::engine::bridge_insert_tests::bridge_effect_track;
use crate::runtime::{
    RuntimeAutomationCurve, RuntimeAutomationLane, RuntimeAutomationPoint, RuntimeAutomationTarget,
};
use crate::types::EngineDualPan;

const FRAMES: usize = 8;

/// A channel with no inserts whose block holds `l` / `r` on every sample.
fn track(l: f32, r: f32) -> RuntimeTrack {
    let mut track = bridge_effect_track(0.0);
    track.inserts.clear();
    track.volume = 1.0;
    track.pan = 0.0;
    track.block_l = vec![l; FRAMES];
    track.block_r = vec![r; FRAMES];
    track
}

fn refill(track: &mut RuntimeTrack, l: f32, r: f32) {
    track.block_l.iter_mut().for_each(|s| *s = l);
    track.block_r.iter_mut().for_each(|s| *s = r);
}

fn dual(left: f32, right: f32) -> EngineDualPan {
    EngineDualPan {
        enabled: true,
        left,
        right,
    }
}

fn lane(target: RuntimeAutomationTarget, value: f32) -> RuntimeAutomationLane {
    RuntimeAutomationLane {
        id: "auto-1".to_string(),
        name: "Automation".to_string(),
        target,
        enabled: true,
        points: vec![RuntimeAutomationPoint {
            beat: 0.0,
            value,
            curve: RuntimeAutomationCurve::Linear,
            tension: 0.0,
        }],
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1.0e-6
}

/// A channel that never engages Dual is processed by the balance ramp and
/// nothing else: the same samples, bit for bit, as the kernel alone.
#[test]
fn balance_is_untouched_while_dual_is_off() {
    let mut t = track(0.8, -0.3);
    t.volume = 0.7;
    t.pan = -0.4;
    t.smoothed_gain_l = 0.2;
    t.smoothed_gain_r = 0.9;
    // Positions set but the mode off: they must not matter.
    t.dual_pan.settings = EngineDualPan {
        enabled: false,
        left: 0.5,
        right: -0.5,
    };

    let mut expect_l = vec![0.8f32; FRAMES];
    let mut expect_r = vec![-0.3f32; FRAMES];
    let (pan_l, pan_r) = pan_gains(-0.4);
    let (target_l, target_r) = (0.7 * pan_l, 0.7 * pan_r);
    let inv = 1.0 / FRAMES as f32;
    simd::ramp_gain_stereo(
        &mut expect_l,
        &mut expect_r,
        0.2,
        (target_l - 0.2) * inv,
        0.9,
        (target_r - 0.9) * inv,
    );

    apply_fader(&mut t, FRAMES, 0.0, true);

    for i in 0..FRAMES {
        assert_eq!(
            t.block_l[i].to_bits(),
            expect_l[i].to_bits(),
            "L sample {i}"
        );
        assert_eq!(
            t.block_r[i].to_bits(),
            expect_r[i].to_bits(),
            "R sample {i}"
        );
    }
    assert!(!t.dual_pan.engaged);
    assert_eq!((t.smoothed_gain_l, t.smoothed_gain_r), (target_l, target_r));
}

/// The default placement passes the pair through; moving the inputs mixes
/// them through the matrix.
#[test]
fn the_matrix_places_each_input() {
    // Defaults: identity.
    let mut t = track(0.5, -0.25);
    t.dual_pan.settings = dual(-1.0, 1.0);
    apply_fader(&mut t, FRAMES, 0.0, false);
    assert!(t.block_l.iter().all(|s| *s == 0.5));
    assert!(t.block_r.iter().all(|s| *s == -0.25));

    // Both inputs hard left at half volume: everything on the left.
    let mut t = track(0.5, 0.25);
    t.volume = 0.5;
    t.dual_pan.settings = dual(-1.0, -1.0);
    apply_fader(&mut t, FRAMES, 0.0, false);
    assert!(t.block_l.iter().all(|s| close(*s, 0.375)));
    assert!(t.block_r.iter().all(|s| *s == 0.0));

    // Swapped: left input hard right, right input hard left.
    let mut t = track(1.0, 0.0);
    t.dual_pan.settings = dual(1.0, -1.0);
    apply_fader(&mut t, FRAMES, 0.0, false);
    assert!(t.block_l.iter().all(|s| *s == 0.0));
    assert!(t.block_r.iter().all(|s| *s == 1.0));

    // A centred left input sends -3 dB to each side.
    let mut t = track(1.0, 0.0);
    t.dual_pan.settings = dual(0.0, 1.0);
    apply_fader(&mut t, FRAMES, 0.0, false);
    let half_power = std::f32::consts::FRAC_1_SQRT_2;
    assert!(t.block_l.iter().all(|s| close(*s, half_power)));
    assert!(t.block_r.iter().all(|s| close(*s, half_power)));
}

/// Entering Dual ramps from the balance gains across one block, ends on
/// the matrix, and holds it from the next block on.
#[test]
fn entering_dual_ramps_from_the_balance_gains() {
    let mut t = track(1.0, 0.0);
    t.smoothed_gain_l = 1.0;
    t.smoothed_gain_r = 1.0;
    // Swap: the left input goes hard right.
    t.dual_pan.settings = dual(1.0, -1.0);
    apply_fader(&mut t, FRAMES, 0.0, true);

    // Start [1, 0, 0, 1] → target [0, 1, 1, 0]; with R silent, out_l is the
    // l→L gain and out_r the l→R gain.
    for i in 0..FRAMES {
        let step = i as f32 / FRAMES as f32;
        assert!(
            close(t.block_l[i], 1.0 - step),
            "L sample {i}: {}",
            t.block_l[i]
        );
        assert!(close(t.block_r[i], step), "R sample {i}: {}", t.block_r[i]);
    }
    assert!(t.dual_pan.engaged);
    assert_eq!(t.dual_pan.matrix, [0.0, 1.0, 1.0, 0.0]);

    refill(&mut t, 1.0, 0.0);
    apply_fader(&mut t, FRAMES, 0.0, true);
    assert!(t.block_l.iter().all(|s| *s == 0.0));
    assert!(t.block_r.iter().all(|s| *s == 1.0));
}

/// Leaving Dual ramps the matrix back to the balance gains over one block,
/// then hands over to the balance smoother at exactly those gains.
#[test]
fn leaving_dual_ramps_back_to_balance() {
    let mut t = track(1.0, 0.0);
    t.dual_pan = crate::runtime::RuntimeDualPan::seeded(dual(1.0, -1.0), 1.0);
    assert_eq!(t.dual_pan.matrix, [0.0, 1.0, 1.0, 0.0]);

    t.dual_pan.settings.enabled = false;
    apply_fader(&mut t, FRAMES, 0.0, true);
    for i in 0..FRAMES {
        let step = i as f32 / FRAMES as f32;
        assert!(close(t.block_l[i], step), "L sample {i}: {}", t.block_l[i]);
        assert!(
            close(t.block_r[i], 1.0 - step),
            "R sample {i}: {}",
            t.block_r[i]
        );
    }
    assert!(!t.dual_pan.engaged);
    assert_eq!((t.smoothed_gain_l, t.smoothed_gain_r), (1.0, 1.0));

    // Back on the balance path: centred, unity.
    refill(&mut t, 1.0, 0.0);
    apply_fader(&mut t, FRAMES, 0.0, true);
    assert!(t.block_l.iter().all(|s| *s == 1.0));
    assert!(t.block_r.iter().all(|s| *s == 0.0));
}

/// Dual pan automation moves the positions while the channel is in Dual,
/// and does nothing on a Balance channel.
#[test]
fn dual_pan_automation_drives_the_positions() {
    let mut t = track(1.0, 1.0);
    t.dual_pan.settings = dual(-1.0, 1.0);
    // Both lanes at hard left (normalized 0).
    t.automation_lanes = vec![
        lane(RuntimeAutomationTarget::TrackPanLeft, 0.0),
        lane(RuntimeAutomationTarget::TrackPanRight, 0.0),
    ];
    apply_fader(&mut t, FRAMES, 0.0, false);
    assert!(t.block_l.iter().all(|s| close(*s, 2.0)));
    assert!(t.block_r.iter().all(|s| *s == 0.0));

    let mut t = track(1.0, 1.0);
    t.automation_lanes = vec![lane(RuntimeAutomationTarget::TrackPanRight, 0.0)];
    apply_fader(&mut t, FRAMES, 0.0, false);
    assert!(t.block_l.iter().all(|s| *s == 1.0));
    assert!(t.block_r.iter().all(|s| *s == 1.0));
}

/// The per-sample path places the inputs with the same matrix.
#[test]
fn the_sample_path_uses_the_same_matrix() {
    let mut t = track(0.0, 0.0);
    t.volume = 0.5;
    t.dual_pan.settings = dual(-1.0, -1.0);
    let (l, r) = apply_track_chain_at_beat(0.5, 0.25, &mut t, 0.0);
    assert!(close(l, 0.375));
    assert_eq!(r, 0.0);

    t.dual_pan.settings.enabled = false;
    let (l, r) = apply_track_chain_at_beat(0.5, 0.25, &mut t, 0.0);
    assert_eq!((l, r), (0.25, 0.125));
}

/// A live `SetTrackDualPan` changes the settings only; the matrix the fader
/// applied stays, so the next block ramps from it.
#[test]
fn a_live_update_keeps_the_applied_matrix() {
    let mut runtime = RuntimeProject::default();
    runtime.tracks.push(track(1.0, 1.0));
    runtime.tracks[0].dual_pan = crate::runtime::RuntimeDualPan::seeded(dual(-1.0, 1.0), 1.0);
    runtime.update_track_dual_pan_at(0, dual(5.0, f32::NAN));
    let state = runtime.tracks[0].dual_pan;
    assert_eq!(state.settings, dual(1.0, 1.0));
    assert_eq!(state.matrix, [1.0, 0.0, 0.0, 1.0]);
    assert!(state.engaged);
}

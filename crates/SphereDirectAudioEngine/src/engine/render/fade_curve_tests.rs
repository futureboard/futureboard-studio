//! The crossfade laws, which are the whole point of letting a clip choose one.
//!
//! A crossfade is a *pair* of ramps, so what matters is not the shape of one
//! but what the two sum to across the overlap. These pin both sums, because
//! picking the wrong law is audible as a 3 dB hole or a 3 dB bulge in the
//! middle of every edit — and until now the choice was carried in the project
//! file and ignored by the renderer, so it could not be heard at all.

use super::clip_fade_gain_with_curves;
use crate::runtime::FadeCurve;

const LEN: u64 = 1_000;

/// Gain of a clip fading *in* over the whole of its length, at `t`.
fn rising(curve: FadeCurve, t: f32) -> f32 {
    clip_fade_gain_with_curves((t * LEN as f32) as u64, LEN, LEN, 0, curve, curve)
}

/// Gain of a clip fading *out* over the whole of its length, at `t`.
fn falling(curve: FadeCurve, t: f32) -> f32 {
    clip_fade_gain_with_curves((t * LEN as f32) as u64, LEN, 0, LEN, curve, curve)
}

/// Two uncorrelated takes: their *powers* add, so the power sum has to stay
/// flat or the middle of every crossfade drops about 3 dB.
#[test]
fn equal_power_holds_the_power_sum_flat() {
    for step in 0..=10 {
        let t = step as f32 / 10.0;
        let a = falling(FadeCurve::EqualPower, t);
        let b = rising(FadeCurve::EqualPower, t);
        let power = a * a + b * b;
        assert!(
            (power - 1.0).abs() < 0.01,
            "power sum {power} at t={t} is not flat"
        );
    }
    // The signature of the law: -3 dB, not -6, at the midpoint.
    let mid = rising(FadeCurve::EqualPower, 0.5);
    assert!(
        (mid - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
        "equal-power midpoint {mid} should be ≈0.707"
    );
}

/// Two halves of one continuous performance are perfectly correlated, so
/// their *amplitudes* add. Linear is the law that keeps that sum flat.
#[test]
fn linear_holds_the_amplitude_sum_flat() {
    for step in 0..=10 {
        let t = step as f32 / 10.0;
        let sum = falling(FadeCurve::Linear, t) + rising(FadeCurve::Linear, t);
        assert!(
            (sum - 1.0).abs() < 0.01,
            "amplitude sum {sum} at t={t} is not flat"
        );
    }
    let mid = rising(FadeCurve::Linear, 0.5);
    assert!(
        (mid - 0.5).abs() < 0.01,
        "linear midpoint {mid} should be 0.5"
    );
}

/// The two edges of one clip are independent: a tail crossfading into the
/// next take can be equal power while the head fading up from silence is
/// linear.
#[test]
fn the_two_edges_carry_their_own_curve() {
    let gain = clip_fade_gain_with_curves(
        LEN / 4,
        LEN,
        LEN / 2,
        LEN / 2,
        FadeCurve::Linear,
        FadeCurve::EqualPower,
    );
    // Half way through a linear fade-in, and clear of the fade-out.
    assert!(
        (gain - 0.5).abs() < 0.01,
        "fade-in edge used the wrong law: {gain}"
    );
}

#[test]
fn a_clip_with_no_fades_is_untouched() {
    for curve in [FadeCurve::EqualPower, FadeCurve::Linear] {
        assert_eq!(
            clip_fade_gain_with_curves(LEN / 2, LEN, 0, 0, curve, curve),
            1.0
        );
    }
}

/// The tag crosses a serialization boundary, so an unknown one has to play
/// rather than fail — and it must land on the default, not on silence.
#[test]
fn an_unknown_curve_tag_falls_back_to_equal_power() {
    assert_eq!(FadeCurve::from_tag("linear"), FadeCurve::Linear);
    assert_eq!(FadeCurve::from_tag("Equal_Power"), FadeCurve::EqualPower);
    assert_eq!(
        FadeCurve::from_tag("s-curve-from-2027"),
        FadeCurve::EqualPower
    );
    assert_eq!(FadeCurve::from_tag(""), FadeCurve::EqualPower);
    // And the tag round-trips, so a project saved by this build reloads
    // with the curve it was given.
    for curve in [FadeCurve::EqualPower, FadeCurve::Linear] {
        assert_eq!(FadeCurve::from_tag(curve.tag()), curve);
    }
}

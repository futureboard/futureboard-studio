use super::clip_fade_gain;

#[test]
fn no_fades_is_unity() {
    assert_eq!(clip_fade_gain(0, 1000, 0, 0), 1.0);
    assert_eq!(clip_fade_gain(500, 1000, 0, 0), 1.0);
    assert_eq!(clip_fade_gain(999, 1000, 0, 0), 1.0);
}

#[test]
fn fade_in_ramps_zero_to_one() {
    // 100-sample fade-in over a 1000-sample clip.
    assert_eq!(clip_fade_gain(0, 1000, 100, 0), 0.0);
    assert!((clip_fade_gain(50, 1000, 100, 0) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    // At/after the fade-in length it is full gain.
    assert_eq!(clip_fade_gain(100, 1000, 100, 0), 1.0);
    assert_eq!(clip_fade_gain(900, 1000, 100, 0), 1.0);
}

#[test]
fn fade_out_ramps_one_to_zero() {
    // 100-sample fade-out: starts at sample 900 (duration - fade_out).
    assert_eq!(clip_fade_gain(899, 1000, 0, 100), 1.0);
    assert!((clip_fade_gain(900, 1000, 0, 100) - 1.0).abs() < 1e-6);
    assert!((clip_fade_gain(950, 1000, 0, 100) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    assert!(clip_fade_gain(1000, 1000, 0, 100) <= 0.0);
}

#[test]
fn fade_in_and_out_combine() {
    // In the flat middle region both fades are unity.
    assert!((clip_fade_gain(500, 1000, 100, 100) - 1.0).abs() < 1e-6);
    // Inside the fade-in region only the fade-in shapes the gain.
    let expected = (0.25f32 * std::f32::consts::FRAC_PI_2).sin();
    assert!((clip_fade_gain(25, 1000, 100, 100) - expected).abs() < 1e-6);
}

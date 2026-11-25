#![allow(dead_code)]

/// Apply a linear gain to a sample buffer in-place.
#[inline]
pub fn apply_gain(buffer: &mut [f32], gain: f32) {
    for s in buffer.iter_mut() {
        *s *= gain;
    }
}

/// Convert dBFS to linear amplitude.  Clamps to 0.0 for very negative dB.
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if db <= -120.0 {
        0.0
    } else {
        10.0f32.powf(db / 20.0)
    }
}

/// Convert linear amplitude to dBFS.
#[inline]
pub fn linear_to_db(linear: f32) -> f32 {
    if linear <= 1e-6 {
        -120.0
    } else {
        20.0 * linear.log10()
    }
}

// The master bus does **not** limit, compress, or clip.
//
// There used to be a `soft_limit` here — a `tanh` soft knee from -1.9 dBFS that
// every master sample passed through, on the device path *and* on export. It
// made the engine's output a function of level: a hot mix came back quieter and
// differently shaped than it was mixed, and nothing in the UI said so. (It also
// meant the master meter could never reach 0 dBFS, so the mixer's clip
// indicator never lit — the overload was hidden as well as reshaped.)
//
// A direct engine hands the device exactly what the graph produced. Level
// control belongs to the master fader and to whatever the engineer chose to put
// in the master insert chain — not to the output stage. Samples past full scale
// stay past full scale in `f32`; the only place they are bounded is the
// quantizer that has to fit them into an integer device word or file format
// (see `crate::dsp::dither`), because an integer word cannot represent them.

/// Equal-power stereo pan, unity at center — the sin/cos pan-pot law of an
/// analog console, compensated so a centered channel passes at 0 dB.
///
/// `pan`: -1.0 = full left, 0.0 = center, 1.0 = full right.
/// Returns `(left_gain, right_gain)`:
///
/// * center → `(1.0, 1.0)` — existing mixes keep their level;
/// * the sweep holds constant power (`l² + r² == 2`), so a source does not dip
///   by 3 dB in the middle the way a linear balance law does;
/// * the extremes are exactly `(√2, 0)` / `(0, √2)`: hard-panned material
///   leaves the far speaker completely, +3 dB on the near one keeps the
///   perceived level.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    use std::f32::consts::{FRAC_PI_4, SQRT_2};
    let pan = pan.clamp(-1.0, 1.0);
    // Exact end points: `sin`/`cos` land a few ulps off zero at π/2, which is
    // still a bleed into the far channel.
    if pan <= -1.0 {
        return (SQRT_2, 0.0);
    }
    if pan >= 1.0 {
        return (0.0, SQRT_2);
    }
    if pan == 0.0 {
        return (1.0, 1.0);
    }
    let angle = (pan + 1.0) * FRAC_PI_4; // 0..π/2
    let (r, l) = angle.sin_cos();
    (l * SQRT_2, r * SQRT_2)
}

/// The 2×2 matrix of a Dual pan: where the left input and the right input
/// each land, `[l→L, l→R, r→L, r→R]`, so
///
/// ```text
/// out_l = in_l · m[0] + in_r · m[2]
/// out_r = in_l · m[1] + in_r · m[3]
/// ```
///
/// Each input follows [`pan_gains`]' sin/cos law scaled by `1/√2`: the same
/// constant-power sweep, but unity at the *extremes* rather than at the
/// centre. That is what makes the default placement (left input hard left,
/// right input hard right) exactly the identity, so switching a channel from
/// Balance to Dual changes nothing until a knob moves; a centred input sends
/// −3 dB to each side, as a mono source panned on a console does.
#[inline]
pub fn dual_pan_matrix(left: f32, right: f32) -> [f32; 4] {
    let (ll, lr) = dual_pan_input_gains(left);
    let (rl, rr) = dual_pan_input_gains(right);
    [ll, lr, rl, rr]
}

#[inline]
fn dual_pan_input_gains(pan: f32) -> (f32, f32) {
    use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_4};
    let pan = if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    // Exact end points and centre, as in `pan_gains`.
    if pan <= -1.0 {
        return (1.0, 0.0);
    }
    if pan >= 1.0 {
        return (0.0, 1.0);
    }
    if pan == 0.0 {
        return (FRAC_1_SQRT_2, FRAC_1_SQRT_2);
    }
    let angle = (pan + 1.0) * FRAC_PI_4; // 0..π/2
    let (r, l) = angle.sin_cos();
    (l, r)
}

#[cfg(test)]
mod pan_tests {
    use super::{dual_pan_matrix, pan_gains};

    #[test]
    fn dual_pan_default_is_identity_and_follows_the_pan_law() {
        assert_eq!(dual_pan_matrix(-1.0, 1.0), [1.0, 0.0, 0.0, 1.0]);
        // Both inputs hard left: everything on the left output.
        assert_eq!(dual_pan_matrix(-1.0, -1.0), [1.0, 0.0, 1.0, 0.0]);
        // Out-of-range clamps.
        assert_eq!(dual_pan_matrix(-5.0, 5.0), dual_pan_matrix(-1.0, 1.0));
        for step in -20..=20 {
            let p = step as f32 / 20.0;
            let m = dual_pan_matrix(p, p);
            let (l, r) = pan_gains(p);
            let inv = std::f32::consts::FRAC_1_SQRT_2;
            assert!((m[0] - l * inv).abs() < 1.0e-6, "pan {p}");
            assert!((m[1] - r * inv).abs() < 1.0e-6, "pan {p}");
            assert_eq!((m[0], m[1]), (m[2], m[3]));
            // Constant power per input.
            assert!((m[0] * m[0] + m[1] * m[1] - 1.0).abs() < 1.0e-5);
        }
    }

    #[test]
    fn center_is_unity_and_extremes_isolate_a_channel() {
        assert_eq!(pan_gains(0.0), (1.0, 1.0));
        assert_eq!(pan_gains(-1.0), (std::f32::consts::SQRT_2, 0.0));
        assert_eq!(pan_gains(1.0), (0.0, std::f32::consts::SQRT_2));
        // Out-of-range input clamps instead of overshooting.
        assert_eq!(pan_gains(-3.0), pan_gains(-1.0));
        assert_eq!(pan_gains(3.0), pan_gains(1.0));
    }

    #[test]
    fn sweep_holds_constant_power() {
        for step in -20..=20 {
            let (l, r) = pan_gains(step as f32 / 20.0);
            assert!((l * l + r * r - 2.0).abs() < 1.0e-5, "pan {step}: {l} {r}");
            assert!(l >= 0.0 && r >= 0.0);
        }
        let (l, r) = pan_gains(-0.5);
        assert!(l > r, "left of center favors the left channel");
    }
}

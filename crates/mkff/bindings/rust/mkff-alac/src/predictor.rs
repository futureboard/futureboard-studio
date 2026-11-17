//! ALAC's adaptive FIR predictor (shared by the decoder and the encoder).
//!
//! Sample `j` is predicted from the `num` samples before it, relative to the sample just
//! before those (`top`): `top + (sum(coefs[k] * (x[j-1-k] - top)) + round) >> den_shift`.
//! After each sample the coefficients take a sign-sign LMS step, from the oldest tap towards
//! the newest, until the residual's sign flips. `num` = 31 selects a plain first-order delta.

#[inline]
pub(crate) fn sign_extend(v: i32, bits: u32) -> i32 {
    let s = 32 - bits;
    v.wrapping_shl(s) >> s
}

/// Rounded prediction offset from `top` for the sample after `recent` (oldest first).
#[inline]
fn prediction(coefs: &[i16], recent: &[i32], top: i32, den_half: i32, den_shift: u32) -> i32 {
    let mut sum = 0i32;
    for (c, &s) in coefs.iter().zip(recent.iter().rev()) {
        sum = sum.wrapping_add(i32::from(*c).wrapping_mul(s.wrapping_sub(top)));
    }
    sum.wrapping_add(den_half) >> den_shift
}

/// Coefficient adaptation after a sample with residual `res`.
#[inline]
fn adapt(coefs: &mut [i16], recent: &[i32], top: i32, res: i32, den_shift: u32) {
    let num = coefs.len();
    let mut err = res;
    if res > 0 {
        for k in (0..num).rev() {
            let dd = top.wrapping_sub(recent[num - 1 - k]);
            let sg = dd.signum();
            coefs[k] = coefs[k].wrapping_sub(sg as i16);
            let step = (num - k) as i32;
            err = err.wrapping_sub(step.wrapping_mul(sg.wrapping_mul(dd) >> den_shift));
            if err <= 0 {
                break;
            }
        }
    } else if res < 0 {
        for k in (0..num).rev() {
            let dd = top.wrapping_sub(recent[num - 1 - k]);
            let sg = dd.signum();
            coefs[k] = coefs[k].wrapping_add(sg as i16);
            let step = (num - k) as i32;
            err = err
                .wrapping_sub(step.wrapping_mul(sg.wrapping_neg().wrapping_mul(dd) >> den_shift));
            if err >= 0 {
                break;
            }
        }
    }
}

fn den_half(den_shift: u32) -> i32 {
    if den_shift == 0 {
        0
    } else {
        1 << (den_shift - 1)
    }
}

/// Undo the predictor in place: `buf` holds residuals on entry and samples on return.
/// `coefs` (length `num` unless `num` is 0 or 31) adapt as decoding goes.
pub(crate) fn unpredict(
    buf: &mut [i32],
    coefs: &mut [i16],
    num: usize,
    chan_bits: u32,
    den_shift: u32,
) {
    let n = buf.len();
    if n == 0 || num == 0 {
        return;
    }
    if num == 31 {
        for j in 1..n {
            buf[j] = sign_extend(buf[j].wrapping_add(buf[j - 1]), chan_bits);
        }
        return;
    }
    let coefs = &mut coefs[..num];
    for j in 1..=num.min(n - 1) {
        buf[j] = sign_extend(buf[j].wrapping_add(buf[j - 1]), chan_bits);
    }
    let half = den_half(den_shift);
    for j in num + 1..n {
        let (hist, rest) = buf.split_at_mut(j);
        let top = hist[j - num - 1];
        let recent = &hist[j - num..j];
        let res = rest[0];
        let pred = prediction(coefs, recent, top, half, den_shift);
        rest[0] = sign_extend(res.wrapping_add(top).wrapping_add(pred), chan_bits);
        adapt(coefs, recent, top, res, den_shift);
    }
}

/// The encoder side of [`unpredict`]: residuals of `x` (samples of `chan_bits` bits) into
/// `res`, adapting `coefs` exactly as the decoder will.
pub(crate) fn predict(
    x: &[i32],
    res: &mut Vec<i32>,
    coefs: &mut [i16],
    chan_bits: u32,
    den_shift: u32,
) {
    let n = x.len();
    let num = coefs.len();
    res.clear();
    if n == 0 {
        return;
    }
    res.push(x[0]);
    if num == 0 {
        res.extend_from_slice(&x[1..]);
        return;
    }
    for j in 1..=num.min(n - 1) {
        res.push(sign_extend(x[j].wrapping_sub(x[j - 1]), chan_bits));
    }
    let half = den_half(den_shift);
    for j in num + 1..n {
        let top = x[j - num - 1];
        let recent = &x[j - num..j];
        let pred = prediction(coefs, recent, top, half, den_shift);
        let r = sign_extend(x[j].wrapping_sub(top).wrapping_sub(pred), chan_bits);
        res.push(r);
        adapt(coefs, recent, top, r, den_shift);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predict_unpredict_roundtrip() {
        let x: Vec<i32> = (0..3000)
            .map(|i| ((i as f64 * 0.02).sin() * 30000.0 + (i * 7919 % 101) as f64) as i32)
            .collect();
        for num in [0usize, 1, 4, 8, 16, 30] {
            let init: Vec<i16> = (0..num)
                .map(|k| if k == 0 { 512 } else { (k as i16) * 3 - 9 })
                .collect();
            let mut c = init.clone();
            let mut res = Vec::new();
            predict(&x, &mut res, &mut c, 17, 9);
            let mut c = init.clone();
            let mut buf = res.clone();
            unpredict(&mut buf, &mut c, num, 17, 9);
            assert_eq!(buf, x, "num {num}");
            if num == 8 {
                let big: u64 = res.iter().map(|r| r.unsigned_abs() as u64).sum();
                let raw: u64 = x.iter().map(|r| r.unsigned_abs() as u64).sum();
                assert!(big * 20 < raw, "predictor barely predicts: {big} vs {raw}");
            }
        }
        // the 31 "delta" mode
        let mut buf: Vec<i32> = std::iter::once(x[0])
            .chain(x.windows(2).map(|w| w[1] - w[0]))
            .collect();
        unpredict(&mut buf, &mut [], 31, 17, 0);
        assert_eq!(buf, x);
    }
}

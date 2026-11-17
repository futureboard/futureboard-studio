//! Linear-prediction analysis for the encoder: apodization windows, autocorrelation,
//! Levinson-Durbin recursion and coefficient quantization.

/// Largest LPC order FLAC can code.
pub const MAX_ORDER: usize = 32;

/// A Tukey window of length `n` with taper fraction `p` (0 = rectangular, 1 = Hann), placed over
/// `[start, start + len)` of an `n`-sample window that is zero elsewhere.
pub fn tukey_into(w: &mut Vec<f64>, n: usize, start: usize, len: usize, p: f64) {
    w.clear();
    w.resize(n, 0.0);
    let len = len.min(n - start.min(n));
    let taper = ((p / 2.0) * len as f64) as usize;
    for i in 0..len {
        let v = if taper > 0 && i < taper {
            0.5 - 0.5 * (std::f64::consts::PI * i as f64 / taper as f64).cos()
        } else if taper > 0 && i >= len - taper {
            let j = len - 1 - i;
            0.5 - 0.5 * (std::f64::consts::PI * j as f64 / taper as f64).cos()
        } else {
            1.0
        };
        w[start + i] = v;
    }
}

/// Autocorrelation of `x` for lags `0..=max_lag` into `ac`.
pub fn autocorrelation(x: &[f64], max_lag: usize, ac: &mut [f64]) {
    for (lag, a) in ac.iter_mut().enumerate().take(max_lag + 1) {
        if lag >= x.len() {
            *a = 0.0;
            continue;
        }
        let mut s0 = 0.0;
        let mut s1 = 0.0;
        let (a0, a1) = (&x[lag..], &x[..x.len() - lag]);
        let mut i = 0;
        while i + 1 < a0.len() {
            s0 += a0[i] * a1[i];
            s1 += a0[i + 1] * a1[i + 1];
            i += 2;
        }
        if i < a0.len() {
            s0 += a0[i] * a1[i];
        }
        *a = s0 + s1;
    }
}

/// Levinson-Durbin recursion. For every order `1..=max_order` writes the predictor
/// (`lpc[order - 1][j]` multiplies `x[n - 1 - j]`) and the residual energy `err[order - 1]`.
/// Returns the highest order computed (lower if the recursion becomes unstable).
pub fn levinson(
    ac: &[f64],
    max_order: usize,
    lpc: &mut [[f64; MAX_ORDER]; MAX_ORDER],
    err: &mut [f64; MAX_ORDER],
) -> usize {
    let mut a = [0.0f64; MAX_ORDER];
    let mut e = ac[0];
    if e <= 0.0 {
        return 0;
    }
    for m in 0..max_order {
        // reflection coefficient
        let mut acc = ac[m + 1];
        for j in 0..m {
            acc -= a[j] * ac[m - j];
        }
        let k = acc / e;
        if !k.is_finite() || k.abs() >= 1.0 {
            return m;
        }
        let prev = a;
        a[m] = k;
        for j in 0..m {
            a[j] = prev[j] - k * prev[m - 1 - j];
        }
        e *= 1.0 - k * k;
        lpc[m][..=m].copy_from_slice(&a[..=m]);
        err[m] = e.max(0.0);
        if e <= 0.0 {
            return m + 1;
        }
    }
    max_order
}

/// Quantize predictor coefficients to `precision`-bit signed integers with a right shift of
/// 0..=15. Returns the shift, or `None` if the coefficients are all zero / not finite.
pub fn quantize(coefs: &[f64], precision: u32, q: &mut [i32]) -> Option<u32> {
    let cmax = coefs.iter().fold(0.0f64, |m, c| m.max(c.abs()));
    if cmax <= 0.0 || !cmax.is_finite() {
        return None;
    }
    // largest shift such that cmax * 2^shift fits in precision - 1 magnitude bits
    let log2 = cmax.log2().floor() as i32 + 1;
    let shift = (precision as i32 - 1 - log2).clamp(0, 15) as u32;
    let qmax = (1i32 << (precision - 1)) - 1;
    let qmin = -(1i32 << (precision - 1));
    let scale = f64::from(1u32 << shift);
    let mut carry = 0.0;
    for (c, out) in coefs.iter().zip(q.iter_mut()) {
        // error feedback keeps the sum of quantization errors small
        let v = c * scale + carry;
        let r = v.round().clamp(f64::from(qmin), f64::from(qmax));
        carry = v - r;
        *out = r as i32;
    }
    Some(shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levinson_recovers_ar2() {
        // x[n] = 1.6 x[n-1] - 0.8 x[n-2] + noise
        let mut x = vec![0.0f64; 8192];
        let mut seed = 1u32;
        for i in 2..x.len() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (seed >> 8) as f64 / (1u32 << 24) as f64 - 0.5;
            x[i] = 1.6 * x[i - 1] - 0.8 * x[i - 2] + noise;
        }
        let mut ac = [0.0; 3];
        autocorrelation(&x, 2, &mut ac);
        let mut lpc = [[0.0; MAX_ORDER]; MAX_ORDER];
        let mut err = [0.0; MAX_ORDER];
        assert_eq!(levinson(&ac, 2, &mut lpc, &mut err), 2);
        assert!((lpc[1][0] - 1.6).abs() < 0.02, "{:?}", &lpc[1][..2]);
        assert!((lpc[1][1] + 0.8).abs() < 0.02);
        let mut q = [0i32; 2];
        let shift = quantize(&lpc[1][..2], 12, &mut q).unwrap();
        assert_eq!(shift, 10);
        assert!((f64::from(q[0]) / 1024.0 - lpc[1][0]).abs() < 0.002);
    }

    #[test]
    fn tukey_shape() {
        let mut w = Vec::new();
        tukey_into(&mut w, 100, 0, 100, 0.5);
        assert!(w[0] < 1e-9 && w[99] < 1e-9);
        assert_eq!(w[50], 1.0);
        tukey_into(&mut w, 100, 50, 50, 0.5);
        assert_eq!(w[10], 0.0);
        assert_eq!(w[75], 1.0);
    }
}

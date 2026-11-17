//! Mixed-radix complex FFT for any length.
//!
//! Decimation in time over the factors of `n` (4s first, then 2, 3, 5 and any remaining primes),
//! with dedicated radix-2 / 4 butterflies and a generic one for the rest. Twiddles are computed
//! in f64.

use std::ops::{Add, AddAssign, Mul, Neg, Sub};

/// A complex number in f32.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex {
    pub re: f32,
    pub im: f32,
}

impl Complex {
    pub const ZERO: Complex = Complex { re: 0.0, im: 0.0 };

    #[inline]
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }

    /// `e^{iθ}`.
    #[inline]
    pub fn expi(theta: f64) -> Self {
        Self {
            re: theta.cos() as f32,
            im: theta.sin() as f32,
        }
    }

    #[inline]
    pub fn conj(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }

    #[inline]
    pub fn scale(self, s: f32) -> Self {
        Self {
            re: self.re * s,
            im: self.im * s,
        }
    }

    #[inline]
    pub fn norm_sqr(self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

impl Add for Complex {
    type Output = Complex;
    #[inline]
    fn add(self, o: Complex) -> Complex {
        Complex {
            re: self.re + o.re,
            im: self.im + o.im,
        }
    }
}

impl AddAssign for Complex {
    #[inline]
    fn add_assign(&mut self, o: Complex) {
        self.re += o.re;
        self.im += o.im;
    }
}

impl Sub for Complex {
    type Output = Complex;
    #[inline]
    fn sub(self, o: Complex) -> Complex {
        Complex {
            re: self.re - o.re,
            im: self.im - o.im,
        }
    }
}

impl Mul for Complex {
    type Output = Complex;
    #[inline]
    fn mul(self, o: Complex) -> Complex {
        Complex {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }
}

impl Neg for Complex {
    type Output = Complex;
    #[inline]
    fn neg(self) -> Complex {
        Complex {
            re: -self.re,
            im: -self.im,
        }
    }
}

/// A planned forward / inverse FFT of one length.
///
/// Forward: `X[k] = Σ x[n] e^{-2πi nk/N}`. Inverse: `x[n] = Σ X[k] e^{+2πi nk/N}` (unscaled: a
/// forward then inverse transform multiplies by `N`).
#[derive(Clone, Debug)]
pub struct Fft {
    n: usize,
    /// (radix, remaining length after this stage)
    factors: Vec<(usize, usize)>,
    /// `e^{-2πi k/n}`
    twiddles: Vec<Complex>,
    scratch: Vec<Complex>,
    tmp: Vec<Complex>,
}

impl Fft {
    pub fn new(n: usize) -> Self {
        assert!(n > 0, "FFT length must be positive");
        let twiddles = (0..n)
            .map(|k| Complex::expi(-2.0 * std::f64::consts::PI * k as f64 / n as f64))
            .collect();
        Self {
            n,
            factors: factorize(n),
            twiddles,
            scratch: vec![Complex::ZERO; n],
            tmp: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// In-place forward transform. `data.len()` must equal the planned length.
    pub fn forward(&mut self, data: &mut [Complex]) {
        assert_eq!(data.len(), self.n);
        if self.n == 1 {
            return;
        }
        self.scratch.copy_from_slice(data);
        let mut tmp = std::mem::take(&mut self.tmp);
        work(
            data,
            &self.scratch,
            0,
            1,
            &self.factors,
            &self.twiddles,
            self.n,
            &mut tmp,
        );
        self.tmp = tmp;
    }

    /// In-place inverse transform (unscaled).
    pub fn inverse(&mut self, data: &mut [Complex]) {
        for c in data.iter_mut() {
            *c = c.conj();
        }
        self.forward(data);
        for c in data.iter_mut() {
            *c = c.conj();
        }
    }
}

fn factorize(mut n: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut p = 4;
    while n > 1 {
        while !n.is_multiple_of(p) {
            p = match p {
                4 => 2,
                2 => 3,
                _ => p + 2,
            };
            if p * p > n {
                p = n;
            }
        }
        n /= p;
        out.push((p, n));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn work(
    out: &mut [Complex],
    input: &[Complex],
    offset: usize,
    fstride: usize,
    factors: &[(usize, usize)],
    tw: &[Complex],
    n: usize,
    tmp: &mut Vec<Complex>,
) {
    let (p, m) = factors[0];
    if m == 1 {
        for (j, o) in out.iter_mut().take(p).enumerate() {
            *o = input[offset + j * fstride];
        }
    } else {
        for j in 0..p {
            work(
                &mut out[j * m..(j + 1) * m],
                input,
                offset + j * fstride,
                fstride * p,
                &factors[1..],
                tw,
                n,
                tmp,
            );
        }
    }
    match p {
        2 => bfly2(out, fstride, tw, m),
        4 => bfly4(out, fstride, tw, m),
        _ => bfly_generic(out, fstride, tw, m, p, n, tmp),
    }
}

#[inline]
fn bfly2(out: &mut [Complex], fstride: usize, tw: &[Complex], m: usize) {
    let (a, b) = out.split_at_mut(m);
    for k in 0..m {
        let t = b[k] * tw[k * fstride];
        b[k] = a[k] - t;
        a[k] += t;
    }
}

#[inline]
fn bfly4(out: &mut [Complex], fstride: usize, tw: &[Complex], m: usize) {
    for k in 0..m {
        let s0 = out[k + m] * tw[k * fstride];
        let s1 = out[k + 2 * m] * tw[2 * k * fstride];
        let s2 = out[k + 3 * m] * tw[3 * k * fstride];
        let s5 = out[k] - s1;
        let f0 = out[k] + s1;
        let s3 = s0 + s2;
        let s4 = s0 - s2;
        out[k + 2 * m] = f0 - s3;
        out[k] = f0 + s3;
        out[k + m] = Complex::new(s5.re + s4.im, s5.im - s4.re);
        out[k + 3 * m] = Complex::new(s5.re - s4.im, s5.im + s4.re);
    }
}

fn bfly_generic(
    out: &mut [Complex],
    fstride: usize,
    tw: &[Complex],
    m: usize,
    p: usize,
    n: usize,
    scratch: &mut Vec<Complex>,
) {
    scratch.clear();
    scratch.resize(p, Complex::ZERO);
    for u in 0..m {
        for q1 in 0..p {
            scratch[q1] = out[u + q1 * m];
        }
        for q1 in 0..p {
            let k = u + q1 * m;
            let mut acc = scratch[0];
            let mut twidx = 0usize;
            for s in scratch.iter().skip(1) {
                twidx += fstride * k;
                twidx %= n;
                acc += *s * tw[twidx];
            }
            out[k] = acc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dft(x: &[Complex]) -> Vec<Complex> {
        let n = x.len();
        (0..n)
            .map(|k| {
                let mut acc = (0.0f64, 0.0f64);
                for (j, v) in x.iter().enumerate() {
                    let th = -2.0 * std::f64::consts::PI * (j * k % n) as f64 / n as f64;
                    acc.0 += f64::from(v.re) * th.cos() - f64::from(v.im) * th.sin();
                    acc.1 += f64::from(v.re) * th.sin() + f64::from(v.im) * th.cos();
                }
                Complex::new(acc.0 as f32, acc.1 as f32)
            })
            .collect()
    }

    #[test]
    fn matches_dft() {
        for n in [
            1, 2, 3, 4, 5, 6, 8, 9, 12, 15, 16, 18, 30, 60, 64, 120, 240, 256, 480, 7, 14, 49,
        ] {
            let x: Vec<Complex> = (0..n)
                .map(|i| {
                    Complex::new(
                        ((i * 7 + 3) % 11) as f32 - 5.0,
                        ((i * 5 + 1) % 13) as f32 - 6.0,
                    )
                })
                .collect();
            let want = dft(&x);
            let mut got = x.clone();
            let mut f = Fft::new(n);
            f.forward(&mut got);
            for (a, b) in got.iter().zip(&want) {
                assert!(
                    (a.re - b.re).abs() < 1e-3 * n as f32 && (a.im - b.im).abs() < 1e-3 * n as f32,
                    "n={n}"
                );
            }
            f.inverse(&mut got);
            for (a, b) in got.iter().zip(&x) {
                assert!((a.re / n as f32 - b.re).abs() < 1e-3, "inverse n={n}");
            }
        }
    }
}

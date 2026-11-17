//! MDCT / IMDCT and DCT-IV of any even size, through an `N/2`-point complex FFT.
//!
//! Definitions (unscaled), for `N` coefficients and `2N` time samples:
//!
//! ```text
//! MDCT:   X[k] = Σ_{n=0}^{2N-1} x[n] cos(π/N (n + 1/2 + N/2)(k + 1/2)),   k = 0..N
//! IMDCT:  y[n] = Σ_{k=0}^{N-1}  X[k] cos(π/N (n + 1/2 + N/2)(k + 1/2)),   n = 0..2N
//! DCT-IV: X[k] = Σ_{n=0}^{N-1}  x[n] cos(π/N (n + 1/2)(k + 1/2))
//! ```
//!
//! With a Princen-Bradley window `w` applied before the MDCT and after the IMDCT, overlap-adding
//! consecutive IMDCT outputs scaled by `2/N` reconstructs the input exactly. Codecs whose
//! specifications define the transform with another normalisation scale the coefficients
//! themselves.

use crate::fft::{Complex, Fft};

/// DCT-IV of size `n` (even).
#[derive(Clone, Debug)]
pub struct DctIv {
    n: usize,
    fft: Fft,
    pre: Vec<Complex>,
    post: Vec<Complex>,
    buf: Vec<Complex>,
}

impl DctIv {
    pub fn new(n: usize) -> Self {
        assert!(n >= 2 && n.is_multiple_of(2), "DCT-IV size must be even");
        let m = n / 2;
        let pi = std::f64::consts::PI;
        let pre = (0..m)
            .map(|p| Complex::expi(-pi * p as f64 / n as f64))
            .collect();
        let post = (0..m)
            .map(|q| Complex::expi(-pi * (q as f64 + 0.25) / n as f64))
            .collect();
        Self {
            n,
            fft: Fft::new(m),
            pre,
            post,
            buf: vec![Complex::ZERO; m],
        }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// `output[k] = Σ input[n] cos(π/N (n+½)(k+½))`. Applying it twice multiplies by `N/2`.
    pub fn transform(&mut self, input: &[f32], output: &mut [f32]) {
        let n = self.n;
        let m = n / 2;
        assert!(input.len() >= n && output.len() >= n);
        for p in 0..m {
            self.buf[p] = Complex::new(input[2 * p], input[n - 1 - 2 * p]) * self.pre[p];
        }
        self.fft.forward(&mut self.buf);
        for q in 0..m {
            let c = self.buf[q] * self.post[q];
            output[2 * q] = c.re;
            output[n - 1 - 2 * q] = -c.im;
        }
    }
}

/// MDCT with `n` coefficients (`2n` time samples), `n` even.
#[derive(Clone, Debug)]
pub struct Mdct {
    n: usize,
    dct: DctIv,
    fold: Vec<f32>,
    tmp: Vec<f32>,
}

impl Mdct {
    /// An MDCT producing `n` coefficients from `2n` samples.
    pub fn new(n: usize) -> Self {
        Self {
            n,
            dct: DctIv::new(n),
            fold: vec![0.0; n],
            tmp: vec![0.0; n],
        }
    }

    /// Number of coefficients `N` (the transform reads / writes `2N` samples).
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Forward MDCT: `input` has `2N` samples, `output` receives `N` coefficients.
    pub fn forward(&mut self, input: &[f32], output: &mut [f32]) {
        let n = self.n;
        let h = n / 2;
        assert!(input.len() >= 2 * n && output.len() >= n);
        // (a, b, c, d) -> (-c_R - d, a - b_R)
        for i in 0..h {
            self.fold[i] = -input[3 * h - 1 - i] - input[3 * h + i];
            self.fold[h + i] = input[i] - input[n - 1 - i];
        }
        self.dct.transform(&self.fold, output);
    }

    /// Inverse MDCT: `input` has `N` coefficients, `output` receives `2N` samples (unscaled; see
    /// the module docs).
    pub fn inverse(&mut self, input: &[f32], output: &mut [f32]) {
        let n = self.n;
        let h = n / 2;
        assert!(input.len() >= n && output.len() >= 2 * n);
        self.dct.transform(input, &mut self.tmp);
        let z = &self.tmp;
        output[..h].copy_from_slice(&z[h..2 * h]);
        for i in h..3 * h {
            output[i] = -z[3 * h - 1 - i];
        }
        for i in 3 * h..2 * n {
            output[i] = -z[i - 3 * h];
        }
    }
}

/// Sine window of length `len` (`w[n] = sin(π(n+½)/len)`), Princen-Bradley for any MDCT.
pub fn sine_window(len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| (std::f64::consts::PI * (i as f64 + 0.5) / len as f64).sin() as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_mdct(x: &[f32]) -> Vec<f32> {
        let n = x.len() / 2;
        (0..n)
            .map(|k| {
                let mut s = 0.0f64;
                for (i, v) in x.iter().enumerate() {
                    s += f64::from(*v)
                        * (std::f64::consts::PI / n as f64
                            * (i as f64 + 0.5 + n as f64 / 2.0)
                            * (k as f64 + 0.5))
                            .cos();
                }
                s as f32
            })
            .collect()
    }

    fn direct_imdct(x: &[f32]) -> Vec<f32> {
        let n = x.len();
        (0..2 * n)
            .map(|i| {
                let mut s = 0.0f64;
                for (k, v) in x.iter().enumerate() {
                    s += f64::from(*v)
                        * (std::f64::consts::PI / n as f64
                            * (i as f64 + 0.5 + n as f64 / 2.0)
                            * (k as f64 + 0.5))
                            .cos();
                }
                s as f32
            })
            .collect()
    }

    #[test]
    fn mdct_matches_definition() {
        for n in [4usize, 6, 12, 18, 64, 120, 256, 480, 960] {
            let x: Vec<f32> = (0..2 * n)
                .map(|i| ((i * 37 + 11) % 101) as f32 / 50.0 - 1.0)
                .collect();
            let mut m = Mdct::new(n);
            let mut got = vec![0.0; n];
            m.forward(&x, &mut got);
            let want = direct_mdct(&x);
            for (a, b) in got.iter().zip(&want) {
                assert!(
                    (a - b).abs() < 2e-3 * (n as f32).sqrt(),
                    "mdct n={n}: {a} vs {b}"
                );
            }
            let mut y = vec![0.0; 2 * n];
            m.inverse(&want, &mut y);
            let wanty = direct_imdct(&want);
            for (a, b) in y.iter().zip(&wanty) {
                assert!((a - b).abs() < 5e-3 * n as f32, "imdct n={n}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn tdac_reconstructs() {
        let n = 64;
        let w = sine_window(2 * n);
        let sig: Vec<f32> = (0..n * 6).map(|i| ((i as f32) * 0.37).sin()).collect();
        let mut m = Mdct::new(n);
        let mut out = vec![0.0f32; n * 7];
        let mut coef = vec![0.0; n];
        let mut y = vec![0.0; 2 * n];
        for b in 0..5 {
            let blk: Vec<f32> = (0..2 * n).map(|i| sig[b * n + i] * w[i]).collect();
            m.forward(&blk, &mut coef);
            m.inverse(&coef, &mut y);
            for i in 0..2 * n {
                out[b * n + i] += y[i] * w[i] * 2.0 / n as f32;
            }
        }
        for i in n..5 * n {
            assert!((out[i] - sig[i]).abs() < 1e-4, "sample {i}");
        }
    }
}

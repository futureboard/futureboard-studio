//! The A/52 inverse transform (clause 6.9): a 512-sample IMDCT, or two interleaved 256-sample
//! IMDCTs for block-switched blocks, with the Kaiser-Bessel-derived window and overlap-add.
//!
//! Implemented exactly as the specification's FFT-based procedure (pre-twiddle, N/4- or
//! N/8-point complex IFFT, post-twiddle, windowing / de-interleaving), on the shared
//! [`Fft`](mkff_audio_core::fft::Fft).

use mkff_audio_core::fft::{Complex, Fft};

const N: usize = 512;

/// Kaiser-Bessel-derived window (alpha = 5) of Table 6.33, first half (256 values).
pub(crate) fn kbd_window() -> [f32; 256] {
    fn bessel_i0(x: f64) -> f64 {
        let mut sum = 1.0;
        let mut term = 1.0;
        for k in 1..64 {
            let t = x / (2.0 * k as f64);
            term *= t * t;
            sum += term;
            if term < sum * 1e-18 {
                break;
            }
        }
        sum
    }
    let alpha = 5.0f64;
    let kernel: Vec<f64> = (0..=N / 2)
        .map(|n| {
            let r = 4.0 * n as f64 / N as f64 - 1.0;
            bessel_i0(std::f64::consts::PI * alpha * (1.0 - r * r).max(0.0).sqrt())
        })
        .collect();
    let total: f64 = kernel.iter().sum();
    let mut w = [0f32; 256];
    let mut acc = 0.0;
    for (n, v) in w.iter_mut().enumerate() {
        acc += kernel[n];
        *v = (acc / total).sqrt() as f32;
    }
    w
}

/// Inverse transform state shared by all channels of a decoder (the per-channel overlap lives
/// with the channel).
#[derive(Clone, Debug)]
pub(crate) struct Imdct {
    fft128: Fft,
    fft64: Fft,
    tw1: [Complex; 128],
    tw2: [Complex; 64],
    window: [f32; 256],
    z: [Complex; 128],
    z2: [Complex; 64],
    x: [f32; N],
}

impl Imdct {
    pub(crate) fn new() -> Self {
        let mut tw1 = [Complex::ZERO; 128];
        for (k, t) in tw1.iter_mut().enumerate() {
            let a = 2.0 * std::f64::consts::PI * (8 * k + 1) as f64 / (8 * N) as f64;
            *t = Complex::new(-a.cos() as f32, -a.sin() as f32);
        }
        let mut tw2 = [Complex::ZERO; 64];
        for (k, t) in tw2.iter_mut().enumerate() {
            let a = 2.0 * std::f64::consts::PI * (8 * k + 1) as f64 / (4 * N) as f64;
            *t = Complex::new(-a.cos() as f32, -a.sin() as f32);
        }
        Self {
            fft128: Fft::new(128),
            fft64: Fft::new(64),
            tw1,
            tw2,
            window: kbd_window(),
            z: [Complex::ZERO; 128],
            z2: [Complex::ZERO; 64],
            x: [0.0; N],
        }
    }

    /// Transform one block of 256 coefficients, overlap-add with `delay` and write 256 PCM
    /// samples to `out`.
    pub(crate) fn transform(
        &mut self,
        coef: &[f32; 256],
        short: bool,
        delay: &mut [f32; 256],
        out: &mut [f32],
    ) {
        if short {
            self.short_blocks(coef);
        } else {
            self.long_block(coef);
        }
        for n in 0..256 {
            out[n] = 2.0 * (self.x[n] + delay[n]);
            delay[n] = self.x[256 + n];
        }
    }

    fn long_block(&mut self, c: &[f32; 256]) {
        for k in 0..128 {
            let v = Complex::new(c[255 - 2 * k], c[2 * k]);
            self.z[k] = v * self.tw1[k];
        }
        self.fft128.inverse(&mut self.z);
        for n in 0..128 {
            self.z[n] = self.z[n] * self.tw1[n];
        }
        let y = &self.z;
        let w = &self.window;
        let x = &mut self.x;
        for n in 0..64 {
            x[2 * n] = -y[64 + n].im * w[2 * n];
            x[2 * n + 1] = y[63 - n].re * w[2 * n + 1];
            x[128 + 2 * n] = -y[n].re * w[128 + 2 * n];
            x[128 + 2 * n + 1] = y[127 - n].im * w[128 + 2 * n + 1];
            x[256 + 2 * n] = -y[64 + n].re * w[255 - 2 * n];
            x[256 + 2 * n + 1] = y[63 - n].im * w[254 - 2 * n];
            x[384 + 2 * n] = y[n].im * w[127 - 2 * n];
            x[384 + 2 * n + 1] = -y[127 - n].re * w[126 - 2 * n];
        }
    }

    fn short_blocks(&mut self, c: &[f32; 256]) {
        // first transform from the even coefficients into z, second from the odd ones into z2
        for k in 0..64 {
            let a = Complex::new(c[2 * (127 - 2 * k)], c[2 * (2 * k)]);
            self.z[k] = a * self.tw2[k];
            let b = Complex::new(c[2 * (127 - 2 * k) + 1], c[2 * (2 * k) + 1]);
            self.z2[k] = b * self.tw2[k];
        }
        self.fft64.inverse(&mut self.z[..64]);
        self.fft64.inverse(&mut self.z2);
        for n in 0..64 {
            self.z[n] = self.z[n] * self.tw2[n];
            self.z2[n] = self.z2[n] * self.tw2[n];
        }
        let y1 = &self.z;
        let y2 = &self.z2;
        let w = &self.window;
        let x = &mut self.x;
        for n in 0..64 {
            x[2 * n] = -y1[n].im * w[2 * n];
            x[2 * n + 1] = y1[63 - n].re * w[2 * n + 1];
            x[128 + 2 * n] = -y1[n].re * w[128 + 2 * n];
            x[128 + 2 * n + 1] = y1[63 - n].im * w[128 + 2 * n + 1];
            x[256 + 2 * n] = -y2[n].re * w[255 - 2 * n];
            x[256 + 2 * n + 1] = y2[63 - n].im * w[254 - 2 * n];
            x[384 + 2 * n] = y2[n].im * w[127 - 2 * n];
            x[384 + 2 * n + 1] = -y2[63 - n].re * w[126 - 2 * n];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_matches_table_6_33() {
        let w = kbd_window();
        // a few entries of Table 6.33 (5 decimals)
        let table = [
            (0, 0.00014),
            (10, 0.00220),
            (100, 0.44030),
            (128, 0.71150),
            (255, 1.0),
        ];
        for (i, v) in table {
            assert!((w[i] - v).abs() < 1e-5, "w[{i}] = {}", w[i]);
        }
        // Princen-Bradley: w[n]^2 + w[255-n]^2 = 1 over the 512-sample window's halves
        for n in 0..256 {
            let a = w[n];
            let b = w[255 - n];
            assert!((a * a + b * b - 1.0).abs() < 1e-5);
        }
    }

    /// The forward transform of clause 7.2.3.2 (encoder side), evaluated directly:
    /// `X[k] = -2/N sum x[n] cos(2pi/4N (2n+1)(2k+1) + pi/4 (2k+1)(1+alpha))`.
    fn forward(x: &[f64], alpha: f64) -> Vec<f64> {
        let n = x.len() as f64;
        (0..x.len() / 2)
            .map(|k| {
                let k = k as f64;
                let s: f64 = x
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let i = i as f64;
                        v * (2.0 * std::f64::consts::PI / (4.0 * n)
                            * (2.0 * i + 1.0)
                            * (2.0 * k + 1.0)
                            + std::f64::consts::PI / 4.0 * (2.0 * k + 1.0) * (1.0 + alpha))
                            .cos()
                    })
                    .sum();
                -2.0 / n * s
            })
            .collect()
    }

    /// Encode a signal with the specification's forward transform, switching between long and
    /// short blocks, and check that the decoder's inverse transform reconstructs it (TDAC holds
    /// across block switches).
    #[test]
    fn reconstructs_through_block_switches() {
        let w = kbd_window();
        let win = |n: usize| f64::from(if n < 256 { w[n] } else { w[511 - n] });
        let blocks = 10;
        let sig: Vec<f64> = (0..256 * (blocks + 1))
            .map(|i| {
                let t = i as f64;
                0.3 * (t * 0.05).sin()
                    + 0.2 * (t * 0.71).sin()
                    + if i % 700 == 3 { 0.5 } else { 0.0 }
            })
            .collect();
        let short = [
            false, false, true, true, false, true, false, false, true, false,
        ];
        let mut t = Imdct::new();
        let mut delay = [0f32; 256];
        let mut out = vec![0f32; 256 * blocks];
        for b in 0..blocks {
            let x: Vec<f64> = (0..512).map(|n| sig[b * 256 + n] * win(n)).collect();
            let mut coef = [0f32; 256];
            if short[b] {
                let x1 = forward(&x[..256], -1.0);
                let x2 = forward(&x[256..], 1.0);
                for k in 0..128 {
                    coef[2 * k] = x1[k] as f32;
                    coef[2 * k + 1] = x2[k] as f32;
                }
            } else {
                for (c, v) in coef.iter_mut().zip(forward(&x, 0.0)) {
                    *c = v as f32;
                }
            }
            t.transform(
                &coef,
                short[b],
                &mut delay,
                &mut out[b * 256..(b + 1) * 256],
            );
        }
        // block b outputs input samples b*256 .. b*256+256 (the first block only half)
        for i in 256..256 * blocks {
            assert!(
                (f64::from(out[i]) - sig[i]).abs() < 1e-4,
                "sample {i}: {} vs {}",
                out[i],
                sig[i]
            );
        }
    }
}

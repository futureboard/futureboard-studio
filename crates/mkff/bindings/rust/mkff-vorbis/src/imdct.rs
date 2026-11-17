//! Power-of-two inverse MDCT for the Vorbis block sizes.
//!
//! Same definition (unscaled) as `mkff_audio_core::mdct::Mdct::inverse`:
//! `y[n] = Σ_k X[k] cos(π/N (n + 1/2 + N/2)(k + 1/2))` for `N` coefficients and `2N` outputs,
//! computed as a DCT-IV through an `N/2`-point complex FFT. The core FFT handles any length; this
//! one is specialised to powers of two with split real / imaginary arrays so the butterfly loops
//! vectorise, which roughly halves the decoder's synthesis time.

#[derive(Clone, Debug)]
pub(crate) struct Imdct {
    /// Coefficients `N`.
    n: usize,
    bitrev: Vec<u32>,
    pre: Vec<(f32, f32)>,
    post: Vec<(f32, f32)>,
    /// Twiddles of every radix-2 stage: the stage with half-size `h` uses `[h..2h]`.
    tw_re: Vec<f32>,
    tw_im: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    tmp: Vec<f32>,
}

impl Imdct {
    /// An IMDCT with `n` coefficients (`n` a power of two, at least 4).
    pub fn new(n: usize) -> Self {
        assert!(n >= 4 && n.is_power_of_two());
        let m = n / 2;
        let bits = m.trailing_zeros();
        let bitrev = (0..m as u32)
            .map(|i| {
                if bits == 0 {
                    0
                } else {
                    i.reverse_bits() >> (32 - bits)
                }
            })
            .collect();
        let pi = std::f64::consts::PI;
        let expi = |t: f64| (t.cos() as f32, t.sin() as f32);
        let pre = (0..m).map(|p| expi(-pi * p as f64 / n as f64)).collect();
        let post = (0..m)
            .map(|q| expi(-pi * (q as f64 + 0.25) / n as f64))
            .collect();
        let mut tw_re = vec![0.0; m.max(1)];
        let mut tw_im = vec![0.0; m.max(1)];
        let mut h = 1;
        while h < m {
            for j in 0..h {
                let (c, s) = expi(-pi * j as f64 / h as f64);
                tw_re[h + j] = c;
                tw_im[h + j] = s;
            }
            h *= 2;
        }
        Self {
            n,
            bitrev,
            pre,
            post,
            tw_re,
            tw_im,
            re: vec![0.0; m],
            im: vec![0.0; m],
            tmp: vec![0.0; n],
        }
    }

    /// `input`: `N` coefficients; `output`: `2N` samples.
    pub fn inverse(&mut self, input: &[f32], output: &mut [f32]) {
        let n = self.n;
        let m = n / 2;
        let input = &input[..n];
        let output = &mut output[..2 * n];

        // pre-twiddle into bit-reversed order
        for (p, (&r, &(wr, wi))) in self.bitrev.iter().zip(&self.pre).enumerate() {
            let xr = input[2 * p];
            let xi = input[n - 1 - 2 * p];
            let r = r as usize;
            self.re[r] = xr * wr - xi * wi;
            self.im[r] = xr * wi + xi * wr;
        }

        // radix-2 decimation-in-time stages
        let (re2, _) = self.re.as_chunks_mut::<2>();
        let (im2, _) = self.im.as_chunks_mut::<2>();
        for (re, im) in re2.iter_mut().zip(im2.iter_mut()) {
            let (ar, br) = (re[0], re[1]);
            let (ai, bi) = (im[0], im[1]);
            re[0] = ar + br;
            re[1] = ar - br;
            im[0] = ai + bi;
            im[1] = ai - bi;
        }
        let mut h = 2;
        while h < m {
            let twr = &self.tw_re[h..2 * h];
            let twi = &self.tw_im[h..2 * h];
            for (re, im) in self
                .re
                .chunks_exact_mut(2 * h)
                .zip(self.im.chunks_exact_mut(2 * h))
            {
                let (ar, br) = re.split_at_mut(h);
                let (ai, bi) = im.split_at_mut(h);
                // equal-length slices: no bounds checks, the loop vectorises
                let (ar, ai, br, bi) = (&mut ar[..h], &mut ai[..h], &mut br[..h], &mut bi[..h]);
                let (wr, wi) = (&twr[..h], &twi[..h]);
                for j in 0..h {
                    let tr = br[j] * wr[j] - bi[j] * wi[j];
                    let ti = br[j] * wi[j] + bi[j] * wr[j];
                    br[j] = ar[j] - tr;
                    bi[j] = ai[j] - ti;
                    ar[j] += tr;
                    ai[j] += ti;
                }
            }
            h *= 2;
        }

        // post-twiddle: DCT-IV output in `tmp`
        for (q, ((&r, &i), &(wr, wi))) in self.re.iter().zip(&self.im).zip(&self.post).enumerate() {
            self.tmp[2 * q] = r * wr - i * wi;
            self.tmp[n - 1 - 2 * q] = -(r * wi + i * wr);
        }

        // unfold the DCT-IV into the 2N IMDCT outputs
        let h = n / 2;
        let z = &self.tmp;
        output[..h].copy_from_slice(&z[h..n]);
        for (o, &v) in output[h..3 * h].iter_mut().zip(z[..2 * h].iter().rev()) {
            *o = -v;
        }
        for (o, &v) in output[3 * h..].iter_mut().zip(&z[..h]) {
            *o = -v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mkff_audio_core::mdct::Mdct;

    #[test]
    fn matches_core_imdct() {
        for n in [4usize, 8, 16, 32, 64, 128, 512, 1024, 4096] {
            let x: Vec<f32> = (0..n)
                .map(|i| ((i * 37 + 11) % 101) as f32 / 50.0 - 1.0)
                .collect();
            let mut want = vec![0.0; 2 * n];
            Mdct::new(n).inverse(&x, &mut want);
            let mut got = vec![0.0; 2 * n];
            Imdct::new(n).inverse(&x, &mut got);
            let scale = want.iter().fold(0f32, |a, v| a.max(v.abs()));
            for (i, (a, b)) in got.iter().zip(&want).enumerate() {
                assert!((a - b).abs() <= 1e-5 * scale, "n={n} i={i}: {a} vs {b}");
            }
        }
    }
}

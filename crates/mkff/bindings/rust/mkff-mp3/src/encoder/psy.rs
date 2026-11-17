//! A masking model computed on the MDCT spectrum.
//!
//! Per granule and channel: line energies are grouped into partitions about a third of a
//! critical band wide, a tonality estimate per partition comes from the spectral flatness of
//! the surrounding lines, partition energies are spread over the Bark scale (Schroeder's
//! spreading function), and the masking threshold is the spread energy lowered by an offset
//! between 6 dB (noise maskers) and about 25 dB (tonal maskers). The threshold is limited from
//! below by the absolute threshold of hearing and, for long blocks, from above by twice the
//! previous granule's threshold (pre-echo control). The allowed noise per scale factor band
//! drives the quantizer; the perceptual entropy drives the bit allocation.

use crate::tables::SfBands;

/// Bark scale (Zwicker / Terhardt approximation).
fn bark(f: f64) -> f64 {
    13.0 * (0.00076 * f).atan() + 3.5 * ((f / 7500.0) * (f / 7500.0)).atan()
}

/// Absolute threshold of hearing in dB SPL (Terhardt).
fn ath_db(f: f64) -> f64 {
    let k = (f.max(20.0)) / 1000.0;
    let v = 3.64 * k.powf(-0.8) - 6.5 * (-0.6 * (k - 3.3) * (k - 3.3)).exp() + 1e-3 * k.powi(4);
    v.min(110.0)
}

/// Schroeder's spreading function, linear power, for `dz = z(maskee) - z(masker)`.
fn spreading(dz: f64) -> f64 {
    if !(-3.0..=8.0).contains(&dz) {
        return 0.0;
    }
    let x = dz + 0.474;
    let db = 15.81 + 7.5 * x - 17.5 * (1.0 + x * x).sqrt();
    10f64.powf(db / 10.0)
}

/// Partitioning of one block size.
struct Partitions {
    start: Vec<usize>,
    end: Vec<usize>,
    /// Spreading matrix `[maskee][masker]` (row-major, non-zero range per row).
    spread: Vec<f32>,
    /// Masker range per maskee row.
    range: Vec<(usize, usize)>,
    /// `1 / Σ spreading` per maskee (normalisation for a flat spectrum).
    norm: Vec<f32>,
    /// Tonal masking offset per partition, linear power factor.
    tmn: Vec<f32>,
    /// Absolute threshold per line, energy.
    ath: Vec<f32>,
}

impl Partitions {
    fn new(lines: usize, rate: u32, ath_scale: f64, lowpass_line: usize) -> Self {
        let df = f64::from(rate) / 2.0 / lines as f64;
        let mut start = Vec::new();
        let mut end = Vec::new();
        let mut i = 0;
        while i < lines {
            let z0 = bark(i as f64 * df);
            let mut j = i + 1;
            while j < lines && bark(j as f64 * df) - z0 < 0.33 {
                j += 1;
            }
            start.push(i);
            end.push(j);
            i = j;
        }
        let n = start.len();
        let center: Vec<f64> = (0..n)
            .map(|p| bark((start[p] + end[p]) as f64 * 0.5 * df))
            .collect();
        let mut spread = vec![0f32; n * n];
        let mut range = vec![(0, 0); n];
        let mut norm = vec![0f32; n];
        for p in 0..n {
            let mut lo = n;
            let mut hi = 0;
            let mut s = 0f64;
            for q in 0..n {
                let v = spreading(center[p] - center[q]);
                spread[p * n + q] = v as f32;
                if v > 0.0 {
                    lo = lo.min(q);
                    hi = hi.max(q + 1);
                    s += v;
                }
            }
            range[p] = (lo, hi.max(lo));
            norm[p] = (1.0 / s.max(1e-9)) as f32;
        }
        let tmn = (0..n)
            .map(|p| {
                let db = (14.5 + center[p]).clamp(18.0, 25.0);
                10f64.powf(-db / 10.0) as f32
            })
            .collect();
        let ath = (0..lines)
            .map(|i| {
                let f = (i as f64 + 0.5) * df;
                let mut a = ath_scale * 10f64.powf((ath_db(f) - 96.0) / 10.0);
                if i >= lowpass_line {
                    a = 1e30;
                }
                a as f32
            })
            .collect();
        Self {
            start,
            end,
            spread,
            range,
            norm,
            tmn,
            ath,
        }
    }

    /// Masking threshold per line of `e` (line energies) into `thr`.
    fn threshold(&self, e: &[f32], thr: &mut [f32], prev: Option<&mut [f32]>, tonal_scale: f32) {
        let n = self.start.len();
        let mut en = [0f32; 128];
        let mut ton = [0f32; 128];
        for p in 0..n {
            let (a, b) = (self.start[p], self.end[p]);
            en[p] = e[a..b].iter().sum();
            // spectral flatness over at least 7 lines around the partition
            let ca = a.saturating_sub(3);
            let cb = (b + 3).min(e.len());
            let mut lsum = 0f32;
            let mut sum = 0f32;
            for &v in &e[ca..cb] {
                let v = v + 1e-20;
                lsum += v.ln();
                sum += v;
            }
            let cnt = (cb - ca) as f32;
            let sfm_db = 4.342_945 * (lsum / cnt - (sum / cnt).ln());
            ton[p] = ((-sfm_db - 6.0) / 14.0).clamp(0.0, 1.0) * tonal_scale;
        }
        let mut pthr = [0f32; 128];
        for p in 0..n {
            let (lo, hi) = self.range[p];
            let row = &self.spread[p * n..p * n + n];
            let mut s = 0f32;
            let mut st = 0f32;
            for q in lo..hi {
                let c = en[q] * row[q];
                s += c;
                st += c * ton[q];
            }
            let tonal = if s > 0.0 { st / s } else { 0.0 };
            // offset: tonal maskers ~18..25 dB, noise maskers 6 dB
            let off = tonal * self.tmn[p] + (1.0 - tonal) * 0.251_188_6;
            pthr[p] = s * self.norm[p] * off;
        }
        if let Some(prev) = prev {
            for p in 0..n {
                let t = pthr[p];
                if prev[p] > 0.0 {
                    pthr[p] = t.min(2.0 * prev[p]);
                }
                prev[p] = t;
            }
        }
        for p in 0..n {
            let (a, b) = (self.start[p], self.end[p]);
            let per_line = pthr[p] / (b - a) as f32;
            for i in a..b {
                thr[i] = per_line.max(self.ath[i]);
            }
        }
    }
}

/// Psychoacoustic model state for all channels.
pub(crate) struct Psy {
    long: Partitions,
    short: Partitions,
    prev_long: Vec<Vec<f32>>,
    tonal_scale: f32,
}

/// Allowed noise and perceptual entropy of one granule of one channel.
#[derive(Clone, Copy)]
pub(crate) struct Masking {
    /// Long: per sfb 0..22; short: per `sfb * 3 + window`.
    pub xmin: [f32; 39],
    /// Signal energy in the same bands.
    pub energy: [f32; 39],
    pub pe: f32,
}

impl Default for Masking {
    fn default() -> Self {
        Self {
            xmin: [0.0; 39],
            energy: [0.0; 39],
            pe: 0.0,
        }
    }
}

impl Psy {
    /// `full_scale` is the MDCT line energy of a full-scale sine (calibrates the absolute
    /// threshold, taken as 96 dB SPL at full scale).
    pub(crate) fn new(rate: u32, channels: usize, full_scale: f64, lowpass_hz: f64) -> Self {
        let lp_long = ((lowpass_hz / (f64::from(rate) / 2.0)) * 576.0).ceil() as usize;
        let lp_short = ((lowpass_hz / (f64::from(rate) / 2.0)) * 192.0).ceil() as usize;
        let long = Partitions::new(576, rate, full_scale, lp_long.min(576));
        // a short-block line covers 3 long lines and the transform has a third of the gain
        let short = Partitions::new(192, rate, full_scale / 3.0, lp_short.min(192));
        let n = long.start.len();
        Self {
            long,
            short,
            prev_long: vec![vec![0.0; n]; channels],
            tonal_scale: 1.0,
        }
    }

    pub(crate) fn reset_prev(&mut self, ch: usize) {
        self.prev_long[ch].iter_mut().for_each(|v| *v = 0.0);
    }

    /// Long block masking for `xr` (576 lines).
    pub(crate) fn long(&mut self, ch: usize, xr: &[f32; 576], bands: &SfBands) -> Masking {
        let mut e = [0f32; 576];
        for i in 0..576 {
            e[i] = xr[i] * xr[i];
        }
        let mut thr = [0f32; 576];
        let prev = &mut self.prev_long[ch];
        self.long
            .threshold(&e, &mut thr, Some(prev), self.tonal_scale);
        let mut m = Masking::default();
        for sfb in 0..22 {
            let (a, b) = (bands.long[sfb] as usize, bands.long[sfb + 1] as usize);
            let en: f32 = e[a..b].iter().sum();
            let t: f32 = thr[a..b].iter().sum();
            m.xmin[sfb] = t;
            m.energy[sfb] = en;
            if en > t {
                m.pe += (b - a) as f32 * 0.5 * (en / t).log2();
            }
        }
        m
    }

    /// Short block masking; `xr` holds the three windows' 192 lines each.
    pub(crate) fn short(&mut self, ch: usize, xr: &[[f32; 192]; 3], bands: &SfBands) -> Masking {
        let mut m = Masking::default();
        for (w, x) in xr.iter().enumerate() {
            let mut e = [0f32; 192];
            for i in 0..192 {
                e[i] = x[i] * x[i];
            }
            let mut thr = [0f32; 192];
            self.short
                .threshold(&e, &mut thr, None, self.tonal_scale * 0.5);
            for sfb in 0..13 {
                let (a, b) = (bands.short[sfb] as usize, bands.short[sfb + 1] as usize);
                let en: f32 = e[a..b].iter().sum();
                let t: f32 = thr[a..b].iter().sum();
                m.xmin[sfb * 3 + w] = t;
                m.energy[sfb * 3 + w] = en;
                if en > t {
                    m.pe += (b - a) as f32 * 0.5 * (en / t).log2();
                }
            }
        }
        // the next long block has no usable history
        self.reset_prev(ch);
        m
    }
}

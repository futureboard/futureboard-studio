//! CELT shape decoding (RFC 6716 §4.3.4–4.3.5): band splitting, PVQ, spreading, folding,
//! time-frequency changes, stereo and anti-collapse. Follows the reference `bands.c`, `vq.c` and
//! `cwrs.c` (float build; see `NOTICE`), including the RFC 8251 hybrid folding update.

// Ported fixed-/floating-point code keeps the reference implementation's expression and loop
// structure, which these lints would obscure.
#![allow(
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::needless_late_init
)]

use std::sync::OnceLock;

use super::rate::{bits2pulses, cache_row, get_pulses, pulses2bits};
use super::tables::{EBANDS, LOG_N};
use super::NB_EBANDS;
use crate::range::{ilog, RangeDecoder, BITRES};

pub const SPREAD_NONE: i32 = 0;
pub const SPREAD_AGGRESSIVE: i32 = 3;
const QTHETA_OFFSET: i32 = 4;
const QTHETA_OFFSET_TWOPHASE: i32 = 16;
const EPSILON: f32 = 1e-15;
/// The largest band (band 20 at LM = 3).
pub const MAX_BAND: usize = 176;

#[inline]
fn eb(i: usize) -> usize {
    EBANDS[i] as usize
}

#[inline]
pub fn lcg_rand(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

#[inline]
fn frac_mul16(a: i32, b: i32) -> i32 {
    (16384 + i32::from(a as i16) * i32::from(b as i16)) >> 15
}

fn bitexact_cos(x: i32) -> i32 {
    let tmp = (4096 + x * x) >> 13;
    let x2 = tmp;
    let x2 = (32767 - x2) + frac_mul16(x2, -7651 + frac_mul16(x2, 8277 + frac_mul16(-626, x2)));
    1 + i32::from(x2 as i16)
}

fn bitexact_log2tan(isin: i32, icos: i32) -> i32 {
    let lc = ilog(icos as u32);
    let ls = ilog(isin as u32);
    let icos = icos << (15 - lc);
    let isin = isin << (15 - ls);
    (ls - lc) * (1 << 11) + frac_mul16(isin, frac_mul16(isin, -2597) + 7932)
        - frac_mul16(icos, frac_mul16(icos, -2597) + 7932)
}

fn isqrt32(x: u32) -> u32 {
    let mut r = (f64::from(x)).sqrt() as u32;
    while u64::from(r) * u64::from(r) > u64::from(x) {
        r -= 1;
    }
    while u64::from(r + 1) * u64::from(r + 1) <= u64::from(x) {
        r += 1;
    }
    r
}

fn compute_qn(n: i32, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> i32 {
    const EXP2_TABLE8: [i32; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];
    let mut n2 = 2 * n - 1;
    if stereo && n == 2 {
        n2 -= 1;
    }
    let mut qb = (b - pulse_cap - (4 << BITRES)).min((b + n2 * offset) / n2);
    qb = qb.min(8 << BITRES);
    if qb < (1 << BITRES >> 1) {
        1
    } else {
        let qn = EXP2_TABLE8[(qb & 7) as usize] >> (14 - (qb >> BITRES));
        (qn + 1) >> 1 << 1
    }
}

// ---------------------------------------------------------------------------------------------
// PVQ codebook (RFC 6716 §4.3.4.2)

const PVQ_MAX_N: usize = MAX_BAND;
const PVQ_MAX_K: usize = 130;

/// `V(N, K)` for N ≤ 176, K ≤ 129 (saturating at `u32::MAX`; every entry the decoder uses fits).
fn pvq_table() -> &'static [u32] {
    static TABLE: OnceLock<Vec<u32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let w = PVQ_MAX_K + 1;
        let mut v = vec![0u32; (PVQ_MAX_N + 1) * w];
        v[0] = 1;
        for n in 1..=PVQ_MAX_N {
            v[n * w] = 1;
            for k in 1..=PVQ_MAX_K {
                let s = u64::from(v[(n - 1) * w + k])
                    + u64::from(v[n * w + k - 1])
                    + u64::from(v[(n - 1) * w + k - 1]);
                v[n * w + k] = s.min(u64::from(u32::MAX)) as u32;
            }
        }
        v
    })
}

#[inline]
fn pvq_v(t: &[u32], n: usize, k: usize) -> u64 {
    u64::from(t[n * (PVQ_MAX_K + 1) + k])
}

/// `decode_pulses`: decode K pulses over N dimensions into `y`.
fn decode_pulses(y: &mut [i32], n: usize, k: usize, ec: &mut RangeDecoder) {
    let t = pvq_table();
    let n = n.min(PVQ_MAX_N);
    let k = k.min(PVQ_MAX_K - 1);
    let total = pvq_v(t, n, k).min(u64::from(u32::MAX)) as u32;
    let mut i = u64::from(ec.uint(total));
    let mut k = k;
    for j in 0..n {
        let nn = n - j;
        let mut p = (pvq_v(t, nn - 1, k) + pvq_v(t, nn, k)) / 2;
        let neg = i >= p;
        if neg {
            i -= p;
        }
        let k0 = k;
        p -= pvq_v(t, nn - 1, k).min(p);
        while p > i && k > 0 {
            k -= 1;
            p -= pvq_v(t, nn - 1, k).min(p);
        }
        let v = (k0 - k) as i32;
        y[j] = if neg { -v } else { v };
        i -= p.min(i);
    }
}

fn exp_rotation1(x: &mut [f32], len: usize, stride: usize, c: f32, s: f32) {
    if len <= stride {
        return;
    }
    for i in 0..len - stride {
        let x1 = x[i];
        let x2 = x[i + stride];
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 - s * x2;
    }
    if len > 2 * stride {
        for i in (0..len - 2 * stride).rev() {
            let x1 = x[i];
            let x2 = x[i + stride];
            x[i + stride] = c * x2 + s * x1;
            x[i] = c * x1 - s * x2;
        }
    }
}

fn exp_rotation(x: &mut [f32], len: usize, dir: i32, stride: usize, k: usize, spread: i32) {
    const SPREAD_FACTOR: [usize; 3] = [15, 10, 5];
    if 2 * k >= len || spread == SPREAD_NONE {
        return;
    }
    let factor = SPREAD_FACTOR[(spread - 1) as usize];
    let gain = len as f32 / (len + factor * k) as f32;
    let theta = 0.5 * gain * gain;
    let c = (0.5 * std::f32::consts::PI * theta).cos();
    let s = (0.5 * std::f32::consts::PI * (1.0 - theta)).cos();
    let mut stride2 = 0usize;
    if len >= 8 * stride {
        stride2 = 1;
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let len = len / stride;
    for i in 0..stride {
        let xs = &mut x[i * len..(i + 1) * len];
        if dir < 0 {
            if stride2 != 0 {
                exp_rotation1(xs, len, stride2, s, c);
            }
            exp_rotation1(xs, len, 1, c, s);
        } else {
            exp_rotation1(xs, len, 1, c, -s);
            if stride2 != 0 {
                exp_rotation1(xs, len, stride2, s, -c);
            }
        }
    }
}

/// `renormalise_vector`: scale `x` to an L2 norm of `gain`.
pub fn renormalise_vector(x: &mut [f32], gain: f32) {
    let e = EPSILON + x.iter().map(|v| v * v).sum::<f32>();
    let g = gain / e.sqrt();
    for v in x.iter_mut() {
        *v *= g;
    }
}

fn haar1(x: &mut [f32], n0: usize, stride: usize) {
    let n0 = n0 >> 1;
    let s = std::f32::consts::FRAC_1_SQRT_2;
    for i in 0..stride {
        for j in 0..n0 {
            let a = stride * 2 * j + i;
            let b = stride * (2 * j + 1) + i;
            let t1 = s * x[a];
            let t2 = s * x[b];
            x[a] = t1 + t2;
            x[b] = t1 - t2;
        }
    }
}

const ORDERY_TABLE: [usize; 30] = [
    1, 0, 3, 0, 2, 1, 7, 0, 4, 3, 6, 1, 5, 2, 15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5,
];

fn deinterleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let mut tmp = [0f32; MAX_BAND];
    if hadamard {
        let ordery = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[ordery[i] * n0 + j] = x[j * stride + i];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[i * n0 + j] = x[j * stride + i];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn interleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let mut tmp = [0f32; MAX_BAND];
    if hadamard {
        let ordery = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[ordery[i] * n0 + j];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[i * n0 + j];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn stereo_merge(x: &mut [f32], y: &mut [f32], mid: f32) {
    let mut xp = 0f32;
    let mut side = 0f32;
    for (a, b) in x.iter().zip(y.iter()) {
        xp += a * b;
        side += b * b;
    }
    xp *= mid;
    let el = mid * mid + side - 2.0 * xp;
    let er = mid * mid + side + 2.0 * xp;
    if er < 6e-4 || el < 6e-4 {
        y.copy_from_slice(x);
        return;
    }
    let lgain = 1.0 / el.sqrt();
    let rgain = 1.0 / er.sqrt();
    for (a, b) in x.iter_mut().zip(y.iter_mut()) {
        let l = mid * *a;
        let r = *b;
        *a = lgain * (l - r);
        *b = rgain * (l + r);
    }
}

/// Per-frame state shared by the band decoders.
pub struct BandCtx<'a, 'b> {
    pub ec: &'a mut RangeDecoder<'b>,
    pub remaining_bits: i32,
    pub seed: u32,
    pub spread: i32,
    pub intensity: usize,
    pub band: usize,
    pub tf_change: i32,
    pub disable_inv: bool,
}

struct Split {
    inv: bool,
    imid: i32,
    iside: i32,
    delta: i32,
    itheta: i32,
    qalloc: i32,
}

impl BandCtx<'_, '_> {
    #[allow(clippy::too_many_arguments)]
    fn compute_theta(
        &mut self,
        n: usize,
        b: &mut i32,
        bb: i32,
        b0: i32,
        lm: i32,
        stereo: bool,
        fill: &mut u32,
    ) -> Split {
        let i = self.band;
        let n_i = n as i32;
        let pulse_cap = i32::from(LOG_N[i]) + lm * (1 << BITRES);
        let offset = (pulse_cap >> 1)
            - if stereo && n == 2 {
                QTHETA_OFFSET_TWOPHASE
            } else {
                QTHETA_OFFSET
            };
        let mut qn = compute_qn(n_i, *b, offset, pulse_cap, stereo);
        if stereo && i >= self.intensity {
            qn = 1;
        }
        let tell = self.ec.tell_frac() as i32;
        let mut itheta = 0i32;
        let mut inv = false;
        if qn != 1 {
            if stereo && n > 2 {
                let p0 = 3u32;
                let x0 = (qn / 2) as u32;
                let ft = p0 * (x0 + 1) + x0;
                let fs = self.ec.decode(ft);
                let x = if fs < (x0 + 1) * p0 {
                    fs / p0
                } else {
                    x0 + 1 + (fs - (x0 + 1) * p0)
                };
                let (fl, fh) = if x <= x0 {
                    (p0 * x, p0 * (x + 1))
                } else {
                    ((x - 1 - x0) + (x0 + 1) * p0, (x - x0) + (x0 + 1) * p0)
                };
                self.ec.update(fl, fh.min(ft), ft);
                itheta = x as i32;
            } else if b0 > 1 || stereo {
                itheta = self.ec.uint((qn + 1) as u32) as i32;
            } else {
                let half = (qn >> 1) as u32;
                let ft = (half + 1) * (half + 1);
                let fm = self.ec.decode(ft);
                let (fl, fs);
                if fm < (half * (half + 1) >> 1) {
                    let it = (isqrt32(8 * fm + 1) - 1) >> 1;
                    itheta = it as i32;
                    fs = it + 1;
                    fl = it * (it + 1) >> 1;
                } else {
                    let it = (2 * (qn as u32 + 1) - isqrt32(8 * (ft - fm - 1) + 1)) >> 1;
                    itheta = it as i32;
                    fs = qn as u32 + 1 - it;
                    fl = ft - ((qn as u32 + 1 - it) * (qn as u32 + 2 - it) >> 1);
                }
                self.ec.update(fl, (fl + fs).min(ft), ft);
            }
            itheta = itheta.clamp(0, qn) * 16384 / qn;
        } else if stereo {
            if *b > 2 << BITRES && self.remaining_bits > 2 << BITRES {
                inv = self.ec.bit_logp(2);
            }
            if self.disable_inv {
                inv = false;
            }
            itheta = 0;
        }
        let qalloc = self.ec.tell_frac() as i32 - tell;
        *b -= qalloc;
        let (imid, iside, delta);
        if itheta == 0 {
            imid = 32767;
            iside = 0;
            *fill &= (1u32 << bb) - 1;
            delta = -16384;
        } else if itheta == 16384 {
            imid = 0;
            iside = 32767;
            *fill &= ((1u32 << bb) - 1) << bb;
            delta = 16384;
        } else {
            imid = bitexact_cos(itheta);
            iside = bitexact_cos(16384 - itheta);
            delta = frac_mul16((n_i - 1) << 7, bitexact_log2tan(iside, imid));
        }
        Split {
            inv,
            imid,
            iside,
            delta,
            itheta,
            qalloc,
        }
    }

    fn quant_band_n1(
        &mut self,
        x: &mut [f32],
        y: Option<&mut [f32]>,
        lowband_out: Option<&mut [f32]>,
    ) -> u32 {
        let dec_one = |s: &mut Self, v: &mut f32| {
            let mut sign = 0;
            if s.remaining_bits >= 1 << BITRES {
                sign = s.ec.bits(1);
                s.remaining_bits -= 1 << BITRES;
            }
            *v = if sign != 0 { -1.0 } else { 1.0 };
        };
        dec_one(self, &mut x[0]);
        if let Some(y) = y {
            dec_one(self, &mut y[0]);
        }
        if let Some(lo) = lowband_out {
            lo[0] = x[0];
        }
        1
    }

    /// Mono recursion (`quant_partition`).
    #[allow(clippy::too_many_arguments)]
    fn quant_partition(
        &mut self,
        x: &mut [f32],
        n: usize,
        mut b: i32,
        mut bb: i32,
        lowband: Option<&mut [f32]>,
        mut lm: i32,
        gain: f32,
        mut fill: u32,
    ) -> u32 {
        let i = self.band;
        let b0 = bb;
        let cache = cache_row(i, lm);
        if lm != -1 && b > i32::from(cache[cache[0] as usize]) + 12 && n > 2 {
            let n = n >> 1;
            let (xa, xb) = x.split_at_mut(n);
            lm -= 1;
            if bb == 1 {
                fill = (fill & 1) | (fill << 1);
            }
            bb = (bb + 1) >> 1;
            let sp = self.compute_theta(n, &mut b, bb, b0, lm, false, &mut fill);
            let mid = sp.imid as f32 / 32768.0;
            let side = sp.iside as f32 / 32768.0;
            let mut delta = sp.delta;
            let itheta = sp.itheta;
            if b0 > 1 && (itheta & 0x3fff) != 0 {
                if itheta > 8192 {
                    delta -= delta >> (4 - lm);
                } else {
                    delta = 0.min(delta + ((n as i32) << BITRES >> (5 - lm)));
                }
            }
            let mut mbits = 0.max(b.min((b - delta) / 2));
            let mut sbits = b - mbits;
            self.remaining_bits -= sp.qalloc;
            let (lb_a, lb_b) = match lowband {
                Some(lb) => {
                    let (a, c) = lb.split_at_mut(n);
                    (Some(a), Some(c))
                }
                None => (None, None),
            };
            let mut rebalance = self.remaining_bits;
            let shift = (b0 >> 1) as u32;
            let cm;
            if mbits >= sbits {
                let c1 =
                    self.quant_partition(&mut xa[..n], n, mbits, bb, lb_a, lm, gain * mid, fill);
                rebalance = mbits - (rebalance - self.remaining_bits);
                if rebalance > 3 << BITRES && itheta != 0 {
                    sbits += rebalance - (3 << BITRES);
                }
                let c2 = self.quant_partition(
                    &mut xb[..n],
                    n,
                    sbits,
                    bb,
                    lb_b,
                    lm,
                    gain * side,
                    fill >> bb,
                );
                cm = c1 | (c2 << shift);
            } else {
                let c2 = self.quant_partition(
                    &mut xb[..n],
                    n,
                    sbits,
                    bb,
                    lb_b,
                    lm,
                    gain * side,
                    fill >> bb,
                );
                rebalance = sbits - (rebalance - self.remaining_bits);
                if rebalance > 3 << BITRES && itheta != 16384 {
                    mbits += rebalance - (3 << BITRES);
                }
                let c1 =
                    self.quant_partition(&mut xa[..n], n, mbits, bb, lb_a, lm, gain * mid, fill);
                cm = c1 | (c2 << shift);
            }
            return cm;
        }
        // The basic no-split case.
        let mut q = bits2pulses(i, lm, b);
        let mut curr_bits = pulses2bits(i, lm, q);
        self.remaining_bits -= curr_bits;
        while self.remaining_bits < 0 && q > 0 {
            self.remaining_bits += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(i, lm, q);
            self.remaining_bits -= curr_bits;
        }
        let x = &mut x[..n];
        if q != 0 {
            let k = get_pulses(q) as usize;
            let mut iy = [0i32; MAX_BAND];
            decode_pulses(&mut iy[..n], n, k, self.ec);
            let ryy: f32 = iy[..n].iter().map(|&v| (v * v) as f32).sum();
            let g = gain / ryy.max(EPSILON).sqrt();
            for (xv, &yv) in x.iter_mut().zip(iy.iter()) {
                *xv = g * yv as f32;
            }
            exp_rotation(x, n, -1, bb as usize, k, self.spread);
            // extract_collapse_mask
            if bb <= 1 {
                return 1;
            }
            let n0 = n / bb as usize;
            let mut mask = 0u32;
            for blk in 0..bb as usize {
                if iy[blk * n0..(blk + 1) * n0].iter().any(|&v| v != 0) {
                    mask |= 1 << blk;
                }
            }
            return mask;
        }
        let cm_mask = ((1u64 << bb) - 1) as u32;
        fill &= cm_mask;
        if fill == 0 {
            x.fill(0.0);
            return 0;
        }
        let cm;
        match lowband {
            None => {
                for v in x.iter_mut() {
                    self.seed = lcg_rand(self.seed);
                    *v = ((self.seed as i32) >> 20) as f32;
                }
                cm = cm_mask;
            }
            Some(lb) => {
                for (v, l) in x.iter_mut().zip(lb.iter()) {
                    self.seed = lcg_rand(self.seed);
                    let tmp = if self.seed & 0x8000 != 0 {
                        1.0 / 256.0
                    } else {
                        -1.0 / 256.0
                    };
                    *v = l + tmp;
                }
                cm = fill;
            }
        }
        renormalise_vector(x, gain);
        cm
    }

    /// Mono band at level 0 (`quant_band`): TF changes, Hadamard reordering, folding output.
    #[allow(clippy::too_many_arguments)]
    fn quant_band(
        &mut self,
        x: &mut [f32],
        n: usize,
        b: i32,
        mut bb: i32,
        mut lowband: Option<&mut [f32]>,
        lm: i32,
        lowband_out: Option<&mut [f32]>,
        gain: f32,
        mut fill: u32,
    ) -> u32 {
        const BIT_INTERLEAVE: [u32; 16] = [0, 1, 1, 1, 2, 3, 3, 3, 2, 3, 3, 3, 2, 3, 3, 3];
        const BIT_DEINTERLEAVE: [u32; 16] = [
            0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3,
            0xFC, 0xFF,
        ];
        let n0 = n;
        let mut n_b = n / bb as usize;
        let long_blocks = bb == 1;
        let mut tf_change = self.tf_change;
        let mut time_divide = 0;
        let mut recombine = 0;
        if n == 1 {
            return self.quant_band_n1(x, None, lowband_out);
        }
        if tf_change > 0 {
            recombine = tf_change;
        }
        for k in 0..recombine {
            if let Some(lb) = lowband.as_deref_mut() {
                haar1(lb, n >> k, 1 << k);
            }
            fill = BIT_INTERLEAVE[(fill & 0xF) as usize]
                | BIT_INTERLEAVE[((fill >> 4) & 0xF) as usize] << 2;
        }
        bb >>= recombine;
        n_b <<= recombine;
        while n_b & 1 == 0 && tf_change < 0 {
            if let Some(lb) = lowband.as_deref_mut() {
                haar1(lb, n_b, bb as usize);
            }
            fill |= fill << bb;
            bb <<= 1;
            n_b >>= 1;
            time_divide += 1;
            tf_change += 1;
        }
        let b0 = bb;
        let n_b0 = n_b;
        if b0 > 1 {
            if let Some(lb) = lowband.as_deref_mut() {
                deinterleave_hadamard(
                    lb,
                    n_b >> recombine,
                    (b0 << recombine) as usize,
                    long_blocks,
                );
            }
        }
        let mut cm = self.quant_partition(x, n, b, bb, lowband, lm, gain, fill);
        if b0 > 1 {
            interleave_hadamard(x, n_b >> recombine, (b0 << recombine) as usize, long_blocks);
        }
        let mut n_b = n_b0;
        bb = b0;
        for _ in 0..time_divide {
            bb >>= 1;
            n_b <<= 1;
            cm |= cm >> bb;
            haar1(x, n_b, bb as usize);
        }
        for k in 0..recombine {
            cm = BIT_DEINTERLEAVE[(cm & 0xF) as usize];
            haar1(x, n0 >> k, 1 << k);
        }
        bb <<= recombine;
        if let Some(lo) = lowband_out {
            let s = (n0 as f32).sqrt();
            for (o, v) in lo.iter_mut().zip(x.iter()) {
                *o = s * v;
            }
        }
        cm & ((1u64 << bb) - 1) as u32
    }

    /// Stereo band (`quant_band_stereo`).
    #[allow(clippy::too_many_arguments)]
    fn quant_band_stereo(
        &mut self,
        x: &mut [f32],
        y: &mut [f32],
        n: usize,
        mut b: i32,
        bb: i32,
        lowband: Option<&mut [f32]>,
        lm: i32,
        lowband_out: Option<&mut [f32]>,
        mut fill: u32,
    ) -> u32 {
        if n == 1 {
            return self.quant_band_n1(x, Some(y), lowband_out);
        }
        let orig_fill = fill;
        let sp = self.compute_theta(n, &mut b, bb, bb, lm, true, &mut fill);
        let mid = sp.imid as f32 / 32768.0;
        let side = sp.iside as f32 / 32768.0;
        let itheta = sp.itheta;
        let cm;
        if n == 2 {
            let mut mbits = b;
            let mut sbits = 0;
            if itheta != 0 && itheta != 16384 {
                sbits = 1 << BITRES;
            }
            mbits -= sbits;
            let c = itheta > 8192;
            self.remaining_bits -= sp.qalloc + sbits;
            let mut sign = 0;
            if sbits != 0 {
                sign = self.ec.bits(1) as i32;
            }
            let sign = (1 - 2 * sign) as f32;
            if c {
                cm = self.quant_band(y, n, mbits, bb, lowband, lm, lowband_out, 1.0, orig_fill);
                x[0] = -sign * y[1];
                x[1] = sign * y[0];
            } else {
                cm = self.quant_band(x, n, mbits, bb, lowband, lm, lowband_out, 1.0, orig_fill);
                y[0] = -sign * x[1];
                y[1] = sign * x[0];
            }
            x[0] *= mid;
            x[1] *= mid;
            y[0] *= side;
            y[1] *= side;
            for k in 0..2 {
                let t = x[k];
                x[k] = t - y[k];
                y[k] += t;
            }
        } else {
            let mut mbits = 0.max(b.min((b - sp.delta) / 2));
            let mut sbits = b - mbits;
            self.remaining_bits -= sp.qalloc;
            let mut rebalance = self.remaining_bits;
            if mbits >= sbits {
                let c1 = self.quant_band(x, n, mbits, bb, lowband, lm, lowband_out, 1.0, fill);
                rebalance = mbits - (rebalance - self.remaining_bits);
                if rebalance > 3 << BITRES && itheta != 0 {
                    sbits += rebalance - (3 << BITRES);
                }
                let c2 = self.quant_band(y, n, sbits, bb, None, lm, None, side, fill >> bb);
                cm = c1 | c2;
            } else {
                let c2 = self.quant_band(y, n, sbits, bb, None, lm, None, side, fill >> bb);
                rebalance = sbits - (rebalance - self.remaining_bits);
                if rebalance > 3 << BITRES && itheta != 16384 {
                    mbits += rebalance - (3 << BITRES);
                }
                let c1 = self.quant_band(x, n, mbits, bb, lowband, lm, lowband_out, 1.0, fill);
                cm = c1 | c2;
            }
        }
        if n != 2 {
            stereo_merge(x, y, mid);
        }
        if sp.inv {
            for v in y.iter_mut() {
                *v = -*v;
            }
        }
        cm
    }
}

/// Inputs of [`quant_all_bands`] beyond the coefficient buffers.
pub struct AllBandsParams<'p> {
    pub start: usize,
    pub end: usize,
    pub pulses: &'p [i32; NB_EBANDS],
    pub short_blocks: bool,
    pub spread: i32,
    pub dual_stereo: bool,
    pub intensity: usize,
    pub tf_res: &'p [i32; NB_EBANDS],
    pub total_bits: i32,
    pub balance: i32,
    pub lm: i32,
    pub coded_bands: usize,
    pub disable_inv: bool,
}

/// Scratch buffers for [`quant_all_bands`] (kept by the decoder to avoid allocation).
pub struct BandScratch {
    norm: Vec<f32>,
    lowband: Vec<f32>,
}

impl BandScratch {
    pub fn new() -> Self {
        BandScratch {
            norm: vec![0.0; 2 * 8 * 100],
            lowband: vec![0.0; MAX_BAND],
        }
    }
}

/// `quant_all_bands` (decoder): decode the normalised spectrum of every band into `x` (and `y`
/// for stereo), each `N = 120 << LM` coefficients long. Returns the RNG seed.
pub fn quant_all_bands(
    p: &AllBandsParams,
    x: &mut [f32],
    mut y: Option<&mut [f32]>,
    collapse_masks: &mut [u8],
    ec: &mut RangeDecoder,
    seed: u32,
    scratch: &mut BandScratch,
) -> u32 {
    let m = 1usize << p.lm;
    let bb: i32 = if p.short_blocks { m as i32 } else { 1 };
    let c = if y.is_some() { 2 } else { 1 };
    let nfull = m * eb(NB_EBANDS);
    let (norm, norm2) = scratch.norm.split_at_mut(nfull);
    let lowband_buf = &mut scratch.lowband;
    let mut lowband_offset = 0usize;
    let mut update_lowband = true;
    let mut dual_stereo = p.dual_stereo;
    let mut balance = p.balance;
    let mut ctx = BandCtx {
        ec,
        remaining_bits: 0,
        seed,
        spread: p.spread,
        intensity: p.intensity,
        band: 0,
        tf_change: 0,
        disable_inv: p.disable_inv,
    };
    for i in p.start..p.end {
        let lo = m * eb(i);
        let hi = m * eb(i + 1);
        let n = hi - lo;
        let tell = ctx.ec.tell_frac() as i32;
        if i != p.start {
            balance -= tell;
        }
        let remaining_bits = p.total_bits - tell - 1;
        ctx.remaining_bits = remaining_bits;
        let b = if i < p.coded_bands {
            let curr_balance = balance / 3.min((p.coded_bands - i) as i32);
            0.max(16383.min((remaining_bits + 1).min(p.pulses[i] + curr_balance)))
        } else {
            0
        };
        if (m * eb(i) >= n + m * eb(p.start) || i == p.start + 1)
            && (update_lowband || lowband_offset == 0)
        {
            lowband_offset = i;
        }
        if i == p.start + 1 {
            // RFC 8251 §9: duplicate the first band so the second can always fold.
            let n1 = m * (eb(p.start + 1) - eb(p.start));
            let n2 = m * (eb(p.start + 2) - eb(p.start + 1));
            let off = m * eb(p.start);
            if n2 > n1 {
                norm.copy_within(off + 2 * n1 - n2..off + n1, off + n1);
                if dual_stereo {
                    norm2.copy_within(off + 2 * n1 - n2..off + n1, off + n1);
                }
            }
        }
        let tf_change = p.tf_res[i];
        ctx.tf_change = tf_change;
        ctx.band = i;
        let mut effective_lowband: Option<usize> = None;
        let (mut x_cm, mut y_cm);
        if lowband_offset != 0 && (p.spread != SPREAD_AGGRESSIVE || bb > 1 || tf_change < 0) {
            let el = (m * eb(p.start)).max((m * eb(lowband_offset)).saturating_sub(n));
            effective_lowband = Some(el);
            let mut fold_start = lowband_offset;
            loop {
                fold_start -= 1;
                if m * eb(fold_start) <= el || fold_start == 0 {
                    break;
                }
            }
            let mut fold_end = lowband_offset - 1;
            loop {
                fold_end += 1;
                if !(fold_end < i && m * eb(fold_end) < el + n) {
                    break;
                }
            }
            x_cm = 0u32;
            y_cm = 0u32;
            let mut fold_i = fold_start;
            loop {
                x_cm |= u32::from(collapse_masks[fold_i * c]);
                y_cm |= u32::from(collapse_masks[fold_i * c + c - 1]);
                fold_i += 1;
                if fold_i >= fold_end {
                    break;
                }
            }
        } else {
            x_cm = ((1u64 << bb) - 1) as u32;
            y_cm = x_cm;
        }
        if dual_stereo && i == p.intensity {
            dual_stereo = false;
            for j in m * eb(p.start)..m * eb(i) {
                norm[j] = 0.5 * (norm[j] + norm2[j]);
            }
        }
        let xb = &mut x[lo..hi];
        let last = i + 1 == p.end;
        // Dual stereo is only signalled for two channels (see `compute_allocation`).
        if let (true, Some(yv)) = (dual_stereo, y.as_deref_mut()) {
            let yb = &mut yv[lo..hi];
            let lb = effective_lowband.map(|el| {
                lowband_buf[..n].copy_from_slice(&norm[el..el + n]);
                &mut lowband_buf[..n]
            });
            let out = if last { None } else { Some(&mut norm[lo..hi]) };
            x_cm = ctx.quant_band(xb, n, b / 2, bb, lb, p.lm, out, 1.0, x_cm);
            let lb = effective_lowband.map(|el| {
                lowband_buf[..n].copy_from_slice(&norm2[el..el + n]);
                &mut lowband_buf[..n]
            });
            let out = if last { None } else { Some(&mut norm2[lo..hi]) };
            y_cm = ctx.quant_band(yb, n, b / 2, bb, lb, p.lm, out, 1.0, y_cm);
        } else {
            let lb = effective_lowband.map(|el| {
                lowband_buf[..n].copy_from_slice(&norm[el..el + n]);
                &mut lowband_buf[..n]
            });
            let out = if last { None } else { Some(&mut norm[lo..hi]) };
            if let Some(yv) = y.as_deref_mut() {
                let yb = &mut yv[lo..hi];
                x_cm = ctx.quant_band_stereo(xb, yb, n, b, bb, lb, p.lm, out, x_cm | y_cm);
            } else {
                x_cm = ctx.quant_band(xb, n, b, bb, lb, p.lm, out, 1.0, x_cm | y_cm);
            }
            y_cm = x_cm;
        }
        collapse_masks[i * c] = x_cm as u8;
        collapse_masks[i * c + c - 1] = y_cm as u8;
        balance += p.pulses[i] + tell;
        update_lowband = b > (n << BITRES) as i32;
    }
    ctx.seed
}

/// `anti_collapse`: fill collapsed short blocks of transient frames with noise.
#[allow(clippy::too_many_arguments)]
pub fn anti_collapse(
    x: &mut [f32],
    collapse_masks: &[u8],
    lm: i32,
    c: usize,
    size: usize,
    start: usize,
    end: usize,
    log_e: &[f32],
    prev1_log_e: &[f32],
    prev2_log_e: &[f32],
    pulses: &[i32; NB_EBANDS],
    mut seed: u32,
) {
    for i in start..end {
        let n0 = eb(i + 1) - eb(i);
        let depth = (1 + pulses[i]) / ((n0 as i32) << lm);
        let thresh = 0.5 * (-0.125 * depth as f32).exp2();
        let sqrt_1 = 1.0 / (((n0 as i32) << lm) as f32).sqrt();
        for ch in 0..c {
            let mut prev1 = prev1_log_e[ch * NB_EBANDS + i];
            let mut prev2 = prev2_log_e[ch * NB_EBANDS + i];
            if c == 1 {
                prev1 = prev1.max(prev1_log_e[NB_EBANDS + i]);
                prev2 = prev2.max(prev2_log_e[NB_EBANDS + i]);
            }
            let ediff = (log_e[ch * NB_EBANDS + i] - prev1.min(prev2)).max(0.0);
            let mut r = 2.0 * (-ediff).exp2();
            if lm == 3 {
                r *= std::f32::consts::SQRT_2;
            }
            r = thresh.min(r) * sqrt_1;
            let xs = &mut x[ch * size + (eb(i) << lm)..ch * size + (eb(i + 1) << lm)];
            let mut renormalize = false;
            for k in 0..1usize << lm {
                if collapse_masks[i * c + ch] & (1 << k) == 0 {
                    for j in 0..n0 {
                        seed = lcg_rand(seed);
                        xs[(j << lm) + k] = if seed & 0x8000 != 0 { r } else { -r };
                    }
                    renormalize = true;
                }
            }
            if renormalize {
                renormalise_vector(xs, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pvq_counts() {
        let t = pvq_table();
        // V(N, 1) = 2N, V(2, K) = 4K, V(N, 2) = 2N^2.
        for n in 1..20 {
            assert_eq!(pvq_v(t, n, 1), 2 * n as u64);
            assert_eq!(pvq_v(t, n, 2), 2 * (n * n) as u64);
        }
        for k in 1..50 {
            assert_eq!(pvq_v(t, 2, k), 4 * k as u64);
        }
        assert_eq!(pvq_v(t, 3, 3), 38);
    }

    #[test]
    fn bitexact_helpers() {
        assert!((32700..=32768).contains(&bitexact_cos(100)));
        assert!((1100..1300).contains(&bitexact_cos(16000)));
        let a = bitexact_cos(8192);
        assert!((a - 23170).abs() < 20, "{a}");
        assert_eq!(bitexact_log2tan(23170, 23170), 0);
        for x in [0u32, 1, 2, 3, 4, 15, 16, 17, 1 << 20, u32::MAX >> 1] {
            let r = isqrt32(x);
            assert!(u64::from(r) * u64::from(r) <= u64::from(x));
            assert!(u64::from(r + 1) * u64::from(r + 1) > u64::from(x));
        }
    }

    #[test]
    fn hadamard_round_trip() {
        for (n0, stride) in [(4usize, 2usize), (2, 4), (3, 8), (1, 16)] {
            for had in [false, true] {
                let orig: Vec<f32> = (0..n0 * stride).map(|v| v as f32).collect();
                let mut x = orig.clone();
                deinterleave_hadamard(&mut x, n0, stride, had);
                interleave_hadamard(&mut x, n0, stride, had);
                assert_eq!(x, orig);
            }
        }
    }
}

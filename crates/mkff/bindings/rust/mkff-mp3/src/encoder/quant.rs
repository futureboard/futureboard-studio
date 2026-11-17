//! Layer III quantization: scale factor bands, the inner (rate) and outer (noise) loops,
//! Huffman table / region selection and scale factor coding.

use crate::huffman::{tables, QA_L, TABLE_INFO};
use crate::layer3::{region_bounds, GranuleInfo};
use crate::tables::{pow43, SfBands, PRETAB, SLEN};

/// Offset of the exponent tables (`q` in quarter steps of 2^(q/4)).
const QOFF: i32 = 320;
const QLEN: usize = 400;
/// Largest quantized magnitude (15 + 2^13 - 1).
const IXMAX: i32 = 8206;

/// One scale factor band of the quantizer, in bitstream line order.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Band {
    pub start: usize,
    pub end: usize,
    /// Long: sfb 0..22; short: sfb 0..13.
    pub sfb: usize,
    /// Short window 0..3; 3 for long blocks.
    pub win: usize,
}

/// The bands of a granule (22 long, or 13 × 3 short in `sfb, window` order).
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    pub bands: Vec<Band>,
    pub short: bool,
}

impl Layout {
    pub(crate) fn new(sf: &SfBands, short: bool) -> Self {
        let mut bands = Vec::new();
        if short {
            for sfb in 0..13 {
                let a = sf.short[sfb] as usize;
                let w = sf.short[sfb + 1] as usize - a;
                for win in 0..3 {
                    let start = 3 * a + win * w;
                    bands.push(Band {
                        start,
                        end: start + w,
                        sfb,
                        win,
                    });
                }
            }
        } else {
            for sfb in 0..22 {
                bands.push(Band {
                    start: sf.long[sfb] as usize,
                    end: sf.long[sfb + 1] as usize,
                    sfb,
                    win: 3,
                });
            }
        }
        Self { bands, short }
    }

    /// Whether band `b` carries a scale factor (the last band does not).
    fn has_sf(&self, b: usize) -> bool {
        if self.short {
            self.bands[b].sfb < 12
        } else {
            b < 21
        }
    }
}

/// Result of quantizing one granule of one channel.
#[derive(Clone)]
pub(crate) struct Quantized {
    pub info: GranuleInfo,
    pub ix: [i32; 576],
    /// Effective amplification (scale factor plus pre-emphasis) per band.
    pub amp: [u8; 39],
    /// Transmitted scale factors.
    pub sf_l: [u8; 22],
    pub sf_s: [[u8; 3]; 13],
    /// MPEG-1: (slen1, slen2, 0, 0); LSF: slen per partition.
    pub slen: [u8; 4],
    pub part2_bits: u32,
    pub part3_bits: u32,
    /// End of the count-1 region (lines).
    pub count1_end: usize,
}

impl Quantized {
    pub(crate) fn empty(block_type: u8) -> Self {
        Self {
            info: GranuleInfo {
                block_type,
                ..GranuleInfo::default()
            },
            ix: [0; 576],
            amp: [0; 39],
            sf_l: [0; 22],
            sf_s: [[0; 3]; 13],
            slen: [0; 4],
            part2_bits: 0,
            part3_bits: 0,
            count1_end: 0,
        }
    }
}

/// Encoder-side tables and settings shared by all granules.
pub(crate) struct Quantizer {
    /// `2^(-3q/16)`: multiplies `|xr|^(3/4)`.
    ipow34: Vec<f32>,
    /// `2^(q/4)`: the dequantization step.
    pow4: Vec<f32>,
    pub lsf: bool,
    pub max_outer: usize,
    pub full_search: bool,
}

/// Bits of the code tables for a region: (table_select, bits without sign bits).
type Choice = (u8, u32);

impl Quantizer {
    pub(crate) fn new(lsf: bool, quality: u8) -> Self {
        let ipow34 = (0..QLEN)
            .map(|i| 2f64.powf(-3.0 * f64::from(i as i32 - QOFF) / 16.0) as f32)
            .collect();
        let pow4 = (0..QLEN)
            .map(|i| 2f64.powf(f64::from(i as i32 - QOFF) / 4.0) as f32)
            .collect();
        let max_outer = match quality {
            0 => 40,
            1 => 30,
            2 => 25,
            3 => 18,
            4 => 14,
            5 => 10,
            6 => 6,
            7 => 4,
            8 => 2,
            _ => 0,
        };
        Self {
            ipow34,
            pow4,
            lsf,
            max_outer,
            full_search: quality <= 6,
        }
    }

    /// Quarter-step exponent of band `b`.
    #[inline]
    fn band_q(&self, gg: i32, amp: u8, ss: bool, short: bool, sbg: u8) -> i32 {
        let shift = 1 + u32::from(ss);
        let mut q = gg - 210 - (i32::from(amp) << shift);
        if short {
            q -= 8 * i32::from(sbg);
        }
        q
    }

    /// Quantize all bands; returns the largest magnitude.
    fn quantize(
        &self,
        xr34: &[f32; 576],
        nz: usize,
        layout: &Layout,
        gg: i32,
        amp: &[u8; 39],
        ss: bool,
        ix: &mut [i32; 576],
    ) -> i32 {
        let mut maxv = 0i32;
        for (b, band) in layout.bands.iter().enumerate() {
            if band.start >= nz {
                ix[band.start..band.end].fill(0);
                continue;
            }
            let q = self.band_q(gg, amp[b], ss, layout.short, 0);
            let m = self.ipow34[(q + QOFF).clamp(0, QLEN as i32 - 1) as usize];
            let end = band.end.min(nz);
            for i in band.start..end {
                let a = xr34[i];
                let v = (a.abs() * m + 0.4054) as i32;
                ix[i] = if a < 0.0 { -v } else { v };

                maxv = maxv.max(v);
            }
            ix[end..band.end].fill(0);
        }
        maxv
    }

    /// Quantization noise energy per band.
    fn noise(
        &self,
        xr: &[f32; 576],
        ix: &[i32; 576],
        layout: &Layout,
        gg: i32,
        amp: &[u8; 39],
        ss: bool,
        out: &mut [f32; 39],
    ) {
        let p43 = pow43();
        for (b, band) in layout.bands.iter().enumerate() {
            let q = self.band_q(gg, amp[b], ss, layout.short, 0);
            let step = self.pow4[(q + QOFF).clamp(0, QLEN as i32 - 1) as usize];
            let mut n = 0f32;
            for i in band.start..band.end {
                let d =
                    xr[i].abs() - p43[(ix[i].unsigned_abs() as usize).min(IXMAX as usize)] * step;
                n += d * d;
            }
            out[b] = n;
        }
    }

    /// Quantize one granule of one channel.
    ///
    /// `xr` is in bitstream order (short blocks: `sfb, window, line`), `xmin` the allowed noise
    /// per band of `layout`. The result uses at most `max_bits` (part 2 + part 3) and, when the
    /// signal allows, at least `min_bits`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn quantize_granule(
        &self,
        xr: &[f32; 576],
        layout: &Layout,
        bands: &SfBands,
        block_type: u8,
        xmin: &[f32; 39],
        max_bits: u32,
        min_bits: u32,
    ) -> Quantized {
        let mut res = Quantized::empty(block_type);
        res.info.block_type = block_type;
        let mut xr34 = [0f32; 576];
        let mut nz = 0;
        for i in 0..576 {
            let a = xr[i].abs();
            let p = a.sqrt() * a.sqrt().sqrt();
            xr34[i] = if xr[i] < 0.0 { -p } else { p };
            if a > 1e-12 {
                nz = i + 1;
            }
        }
        if nz == 0 || max_bits < 8 {
            res.count_bits(bands, false);
            return res;
        }
        let max_xr34 = xr34[..nz].iter().fold(0f32, |m, &v| m.max(v.abs()));
        let nb = layout.bands.len();
        let mut amp = [0u8; 39];
        let mut ss = false;

        // smallest global gain allowed by the largest quantized value
        let gg_floor = (0..=255)
            .find(|&g| {
                let q = g - 210;
                (max_xr34 * self.ipow34[(q + QOFF).clamp(0, QLEN as i32 - 1) as usize] + 0.4054)
                    as i32
                    <= IXMAX
            })
            .unwrap_or(255);

        let mut cand = res.clone();
        let Some(mut gg) = self.inner_loop(
            &xr34, nz, layout, bands, &amp, ss, gg_floor, max_bits, &mut cand,
        ) else {
            res.count_bits(bands, false);
            return res;
        };
        let mut best = cand.clone();
        let mut best_score = f32::INFINITY;
        let mut noise = [0f32; 39];
        for iter in 0..=self.max_outer {
            self.noise(xr, &cand.ix, layout, gg, &amp, ss, &mut noise);
            let mut over = 0usize;
            let mut over_db = 0f32;
            for b in 0..nb {
                let r = noise[b] / xmin[b].max(1e-30);
                if r > 1.0 {
                    over += 1;
                    over_db += 10.0 * r.log10();
                }
            }
            if over_db < best_score {
                best_score = over_db;
                best = cand.clone();
            }
            if over == 0 || iter == self.max_outer {
                break;
            }
            // amplify the bands above their allowed noise
            let mut all = true;
            let mut changed = false;
            for b in 0..nb {
                if !layout.has_sf(b) {
                    continue;
                }
                if noise[b] > xmin[b] {
                    amp[b] += 1;
                    changed = true;
                } else {
                    all = false;
                }
            }
            if all || !changed {
                break;
            }
            if self.scalefactors(&amp, layout, block_type).is_none() {
                if ss {
                    break;
                }
                ss = true;
                for a in amp.iter_mut() {
                    *a = a.div_ceil(2);
                }
                if self.scalefactors(&amp, layout, block_type).is_none() {
                    break;
                }
            }
            let mut c = Quantized::empty(block_type);
            match self.inner_loop(
                &xr34, nz, layout, bands, &amp, ss, gg_floor, max_bits, &mut c,
            ) {
                Some(g) => {
                    gg = g;
                    cand = c;
                }
                None => break,
            }
        }
        let mut res = best;
        // spend bits that would otherwise be stuffed: finer quantization while it fits
        let total = res.part2_bits + res.part3_bits;
        if total < min_bits {
            let amp = res.amp;
            let ss = res.info.scalefac_scale;
            let mut g = res.info.global_gain as i32;
            let mut c = res.clone();
            while g > gg_floor {
                g -= 1;
                let mut t = Quantized {
                    amp,
                    ..Quantized::empty(block_type)
                };
                t.info.scalefac_scale = ss;
                if !self.finish(&xr34, nz, layout, bands, g, &mut t)
                    || t.part2_bits + t.part3_bits > max_bits
                {
                    break;
                }
                c = t;
                if c.part2_bits + c.part3_bits >= min_bits {
                    break;
                }
            }
            res = c;
        }
        if self.full_search {
            res.count_bits(bands, true);
        }
        res
    }

    /// Quantize with `gg` and fill in the side information; `false` when not representable.
    fn finish(
        &self,
        xr34: &[f32; 576],
        nz: usize,
        layout: &Layout,
        bands: &SfBands,
        gg: i32,
        q: &mut Quantized,
    ) -> bool {
        let Some((sfc, preflag, p2, slen)) = self.scalefactors(&q.amp, layout, q.info.block_type)
        else {
            return false;
        };
        let ss = q.info.scalefac_scale;
        let amp = q.amp;
        let maxv = self.quantize(xr34, nz, layout, gg, &amp, ss, &mut q.ix);
        if maxv > IXMAX {
            return false;
        }
        q.info.global_gain = gg as u32;
        q.info.scalefac_compress = sfc;
        q.info.preflag = preflag;
        q.part2_bits = p2;
        q.slen = slen;
        // transmitted scale factors
        if layout.short {
            for (b, band) in layout.bands.iter().enumerate() {
                q.sf_s[band.sfb][band.win] = amp[b];
            }
        } else {
            for sfb in 0..22 {
                let pre = if preflag { PRETAB[sfb] } else { 0 };
                q.sf_l[sfb] = amp[sfb].saturating_sub(pre);
            }
        }
        q.count_bits(bands, false);
        true
    }

    /// Binary search for the smallest global gain whose bits fit `max_bits`.
    #[allow(clippy::too_many_arguments)]
    fn inner_loop(
        &self,
        xr34: &[f32; 576],
        nz: usize,
        layout: &Layout,
        bands: &SfBands,
        amp: &[u8; 39],
        ss: bool,
        gg_floor: i32,
        max_bits: u32,
        out: &mut Quantized,
    ) -> Option<i32> {
        let block_type = out.info.block_type;
        let try_gg = |g: i32, t: &mut Quantized| -> Option<u32> {
            *t = Quantized {
                amp: *amp,
                ..Quantized::empty(block_type)
            };
            t.info.scalefac_scale = ss;
            if self.finish(xr34, nz, layout, bands, g, t) {
                Some(t.part2_bits + t.part3_bits)
            } else {
                None
            }
        };
        // the scale factors raise the effective step of amplified bands; the floor must keep
        // every band within the largest codeable value
        let mut lo = gg_floor;
        let mut t = Quantized::empty(block_type);
        while lo < 255 && try_gg(lo, &mut t).is_none() {
            lo += 1;
        }
        let first = try_gg(lo, &mut t)?;
        if first <= max_bits {
            *out = t;
            return Some(lo);
        }
        let mut hi = 255;
        let mut th = Quantized::empty(block_type);
        match try_gg(hi, &mut th) {
            Some(b) if b <= max_bits => {}
            _ => return None,
        }
        // invariant: lo does not fit, hi fits
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            match try_gg(mid, &mut t) {
                Some(b) if b <= max_bits => {
                    hi = mid;
                    std::mem::swap(&mut th, &mut t);
                }
                _ => lo = mid,
            }
        }
        *out = th;
        Some(hi)
    }

    /// Choose the scale factor coding for `amp`: (`scalefac_compress`, `preflag`, part 2 bits,
    /// slen). `None` when the values cannot be coded.
    pub(crate) fn scalefactors(
        &self,
        amp: &[u8; 39],
        layout: &Layout,
        block_type: u8,
    ) -> Option<(u32, bool, u32, [u8; 4])> {
        let bits_for = |v: u8| -> u8 { (8 - v.leading_zeros()) as u8 };
        if self.lsf {
            // scalefac_compress < 400: partitions {6,5,5,5} (long) / {9,9,9,9} short windows,
            // slen up to 4, 4, 3, 3
            let groups: [(usize, usize); 4] = if layout.short {
                [(0, 9), (9, 18), (18, 27), (27, 36)]
            } else {
                [(0, 6), (6, 11), (11, 16), (16, 21)]
            };
            let mut slen = [0u8; 4];
            for (k, &(a, b)) in groups.iter().enumerate() {
                let m = amp[a..b].iter().copied().max().unwrap_or(0);
                slen[k] = bits_for(m);
                if slen[k] > if k < 2 { 4 } else { 3 } {
                    return None;
                }
            }
            let sfc = ((u32::from(slen[0]) * 5 + u32::from(slen[1])) << 4)
                | (u32::from(slen[2]) << 2)
                | u32::from(slen[3]);
            let p2: u32 = groups
                .iter()
                .zip(&slen)
                .map(|(&(a, b), &s)| (b - a) as u32 * u32::from(s))
                .sum();
            let _ = block_type;
            return Some((sfc, false, p2, slen));
        }
        let (need1, need2, n1, n2, preflag) = if layout.short {
            let m1 = amp[..18].iter().copied().max().unwrap_or(0);
            let m2 = amp[18..36].iter().copied().max().unwrap_or(0);
            (bits_for(m1), bits_for(m2), 18u32, 18u32, false)
        } else {
            let m1 = amp[..11].iter().copied().max().unwrap_or(0);
            let m2 = amp[11..21].iter().copied().max().unwrap_or(0);
            // pre-emphasis when every high band carries at least its pretab value
            let can_pre = (11..21).all(|b| amp[b] >= PRETAB[b]);
            let m2p = (11..21)
                .map(|b| amp[b].saturating_sub(PRETAB[b]))
                .max()
                .unwrap_or(0);
            if can_pre && bits_for(m2p) < bits_for(m2) {
                (bits_for(m1), bits_for(m2p), 11, 10, true)
            } else {
                (bits_for(m1), bits_for(m2), 11, 10, false)
            }
        };
        let mut best: Option<(u32, u32)> = None;
        for (k, &(s1, s2)) in SLEN.iter().enumerate() {
            if s1 >= need1 && s2 >= need2 {
                let bits = n1 * u32::from(s1) + n2 * u32::from(s2);
                if best.is_none_or(|(_, b)| bits < b) {
                    best = Some((k as u32, bits));
                }
            }
        }
        let (k, bits) = best?;
        let (s1, s2) = SLEN[k as usize];
        Some((k, preflag, bits, [s1, s2, 0, 0]))
    }
}

impl Quantized {
    /// Count the part 3 bits of `ix`, choosing the count-1 table, region split (long blocks)
    /// and tables. `full` searches all region splits and tables.
    pub(crate) fn count_bits(&mut self, bands: &SfBands, full: bool) {
        let ix = &self.ix;
        let mut end = 576;
        while end > 0 && ix[end - 1] == 0 {
            end -= 1;
        }
        end += end & 1;
        let mut c1 = end;
        while c1 >= 4 && ix[c1 - 4..c1].iter().all(|&v| v.abs() <= 1) {
            c1 -= 4;
        }
        // count-1 region
        let (mut ca, mut nzc) = (0u32, 0u32);
        for q in ix[c1..end].as_chunks::<4>().0 {
            let mut idx = 0usize;
            for &x in q {
                idx = idx * 2 + usize::from(x != 0);
                nzc += u32::from(x != 0);
            }
            ca += u32::from(QA_L[idx]);
        }
        let quads = ((end - c1) / 4) as u32;
        let cb = 4 * quads;
        self.info.count1_table_b = cb < ca;
        let c1_bits = ca.min(cb) + nzc;
        self.count1_end = c1 + 4 * quads as usize;
        // big values
        let bv = c1 / 2;
        self.info.big_values = bv as u32;
        let mut signs = 0u32;
        for &v in &ix[..c1] {
            signs += u32::from(v != 0);
        }
        let bits = if self.info.block_type != 0 {
            let (r1, _) = region_bounds(&self.info, bands);
            let r1 = r1.min(c1);
            let (t0, b0) = best_table(ix, 0, r1, full);
            let (t1, b1) = best_table(ix, r1, c1, full);
            self.info.table_select = [t0, t1, 0];
            self.info.region0_count = if self.info.block_type == 2 { 8 } else { 7 };
            self.info.region1_count = 36;
            b0 + b1
        } else if full {
            self.search_regions(bands, c1)
        } else {
            let mut nb = 0;
            while nb < 22 && (bands.long[nb] as usize) < c1 {
                nb += 1;
            }
            let r0 = ((nb + 1) / 4).min(15);
            let r1 = nb.div_ceil(3).min(7);
            self.set_regions(bands, c1, r0, r1, false)
        };
        self.part3_bits = bits + signs + c1_bits;
        self.info.part2_3_length = self.part2_bits + self.part3_bits;
    }

    fn set_regions(&mut self, bands: &SfBands, c1: usize, r0: usize, r1: usize, full: bool) -> u32 {
        let a = (bands.long[(r0 + 1).min(22)] as usize).min(c1);
        let b = (bands.long[(r0 + r1 + 2).min(22)] as usize).min(c1);
        let (t0, b0) = best_table(&self.ix, 0, a, full);
        let (t1, b1) = best_table(&self.ix, a, b, full);
        let (t2, b2) = best_table(&self.ix, b, c1, full);
        self.info.region0_count = r0 as u32;
        self.info.region1_count = r1 as u32;
        self.info.table_select = [t0, t1, t2];
        b0 + b1 + b2
    }

    /// Exhaustive region split search with per-band costs.
    fn search_regions(&mut self, bands: &SfBands, c1: usize) -> u32 {
        let t = tables();
        // per long band (clipped to c1): code bits for each of the 15 code tables, escapes and
        // the maximum
        let mut nbands = 0;
        while nbands < 22 && (bands.long[nbands] as usize) < c1 {
            nbands += 1;
        }
        if nbands == 0 {
            self.info.region0_count = 0;
            self.info.region1_count = 0;
            self.info.table_select = [0; 3];
            return 0;
        }
        let mut cost = [[0u32; 23]; 15];
        let mut esc = [0u32; 23];
        let mut maxv = [0i32; 22];
        for sfb in 0..nbands {
            let a = bands.long[sfb] as usize;
            let b = (bands.long[sfb + 1] as usize).min(c1);
            let mut e = 0;
            let mut m = 0;
            let mut c = [0u32; 15];
            for p in (a..b).step_by(2) {
                let x = self.ix[p].unsigned_abs() as usize;
                let y = self.ix[p + 1].unsigned_abs() as usize;
                m = m.max(x.max(y) as i32);
                e += u32::from(x >= 15) + u32::from(y >= 15);
                let k = x.min(15) * 16 + y.min(15);
                for (tb, cc) in c.iter_mut().enumerate() {
                    *cc += u32::from(t.len16[tb][k]);
                }
            }
            for tb in 0..15 {
                cost[tb][sfb + 1] = cost[tb][sfb] + c[tb];
            }
            esc[sfb + 1] = esc[sfb] + e;
            maxv[sfb] = m;
        }
        let region = |a: usize, b: usize| -> Choice {
            if a >= b {
                return (0, 0);
            }
            let m = maxv[a..b].iter().copied().max().unwrap_or(0);
            if m == 0 {
                return (0, 0);
            }
            let mut best: Choice = (0, u32::MAX);
            for (ts, &(tidx, lin)) in TABLE_INFO.iter().enumerate() {
                if tidx == 0 {
                    continue;
                }
                let base = tidx as usize - 1;
                let size = t.size[base] as i32;
                let ok = if lin > 0 {
                    m < 15 + (1 << lin)
                } else {
                    m < size
                };
                if !ok {
                    continue;
                }
                let bits = cost[base][b] - cost[base][a]
                    + if lin > 0 {
                        u32::from(lin) * (esc[b] - esc[a])
                    } else {
                        0
                    };
                if bits < best.1 {
                    best = (ts as u8, bits);
                }
            }
            best
        };
        let mut best = (u32::MAX, 0usize, 0usize);
        for r0 in 0..16usize {
            let a = (r0 + 1).min(nbands);
            let ca = region(0, a);
            for r1 in 0..8usize {
                let b = (r0 + r1 + 2).min(nbands);
                let total = ca.1 + region(a, b).1 + region(b, nbands).1;
                if total < best.0 {
                    best = (total, r0, r1);
                }
                if b == nbands {
                    break;
                }
            }
            if a == nbands {
                break;
            }
        }
        self.set_regions(bands, c1, best.1, best.2, true)
    }
}

/// Best table for lines `a..b` of `ix` (pairs): (table_select, bits without signs).
fn best_table(ix: &[i32; 576], a: usize, b: usize, full: bool) -> Choice {
    if a >= b {
        return (0, 0);
    }
    let m = ix[a..b].iter().map(|v| v.abs()).max().unwrap_or(0);
    if m == 0 {
        return (0, 0);
    }
    let t = tables();
    let eval = |ts: u8| -> u32 {
        let (tidx, lin) = TABLE_INFO[ts as usize];
        let base = tidx as usize - 1;
        let len = &t.len16[base];
        let lin = u32::from(lin);
        let mut bits = 0u32;
        for p in (a..b).step_by(2) {
            let x = ix[p].unsigned_abs() as usize;
            let y = ix[p + 1].unsigned_abs() as usize;
            bits += u32::from(len[x.min(15) * 16 + y.min(15)]);
            if lin > 0 {
                bits += lin * (u32::from(x >= 15) + u32::from(y >= 15));
            }
        }
        bits
    };
    let mut cands: [u8; 16] = [0; 16];
    let mut n = 0;
    let mut push = |ts: u8| {
        cands[n] = ts;
        n += 1;
    };
    if m > 15 {
        let need = 32 - ((m - 15) as u32).leading_zeros();
        if let Some(ts) = (16u8..24).find(|&ts| u32::from(TABLE_INFO[ts as usize].1) >= need) {
            push(ts);
        }
        if let Some(ts) = (24u8..32).find(|&ts| u32::from(TABLE_INFO[ts as usize].1) >= need) {
            push(ts);
        }
    } else if full {
        for ts in [1u8, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15] {
            let size = t.size[TABLE_INFO[ts as usize].0 as usize - 1] as i32;
            if m < size {
                push(ts);
            }
        }
        if m >= 8 {
            push(16);
            push(24);
        }
    } else {
        match m {
            1 => [1u8, 2, 3].iter().for_each(|&t| push(t)),
            2 => [2u8, 3].iter().for_each(|&t| push(t)),
            3 => [5u8, 6].iter().for_each(|&t| push(t)),
            4 | 5 => [7u8, 8, 9].iter().for_each(|&t| push(t)),
            6 | 7 => [10u8, 11, 12].iter().for_each(|&t| push(t)),
            _ => [13u8, 15, 16, 24].iter().for_each(|&t| push(t)),
        }
    }
    let mut best: Choice = (0, u32::MAX);
    for &ts in &cands[..n] {
        let bits = eval(ts);
        if bits < best.1 {
            best = (ts, bits);
        }
    }
    best
}

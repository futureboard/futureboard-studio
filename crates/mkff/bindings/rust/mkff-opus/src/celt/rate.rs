//! CELT bit allocation (RFC 6716 §4.3.3), following the reference `rate.c` (see `NOTICE`).

// Ported fixed-/floating-point code keeps the reference implementation's expression and loop
// structure, which these lints would obscure.
#![allow(
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::needless_late_init
)]

use super::tables::{BAND_ALLOCATION, CACHE_BITS, CACHE_CAPS, CACHE_INDEX, EBANDS, LOG_N};
use super::NB_EBANDS;
use crate::range::{RangeDecoder, BITRES};

const ALLOC_STEPS: i32 = 6;
const MAX_FINE_BITS: i32 = 8;
const FINE_OFFSET: i32 = 21;
const LOG_MAX_PSEUDO: i32 = 6;
const NB_ALLOC_VECTORS: usize = 11;

const LOG2_FRAC_TABLE: [u8; 24] = [
    0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37,
];

#[inline]
fn eb(i: usize) -> i32 {
    i32::from(EBANDS[i])
}

/// The pulse cache row for a band at a given LM (`m->cache.bits + m->cache.index[...]`).
#[inline]
pub fn cache_row(band: usize, lm: i32) -> &'static [u8] {
    let idx = CACHE_INDEX[((lm + 1) as usize) * NB_EBANDS + band];
    &CACHE_BITS[idx.max(0) as usize..]
}

/// `get_pulses`: pseudo-pulse index to pulse count.
#[inline]
pub fn get_pulses(i: i32) -> i32 {
    if i < 8 {
        i
    } else {
        (8 + (i & 7)) << ((i >> 3) - 1)
    }
}

/// `bits2pulses`: the largest pseudo-pulse count that fits in `bits` (1/8 bit).
pub fn bits2pulses(band: usize, lm: i32, bits: i32) -> i32 {
    let cache = cache_row(band, lm);
    let mut lo = 0i32;
    let mut hi = i32::from(cache[0]);
    let bits = bits - 1;
    for _ in 0..LOG_MAX_PSEUDO {
        let mid = (lo + hi + 1) >> 1;
        if i32::from(cache[mid as usize]) >= bits {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let lo_bits = if lo == 0 {
        -1
    } else {
        i32::from(cache[lo as usize])
    };
    if bits - lo_bits <= i32::from(cache[hi as usize]) - bits {
        lo
    } else {
        hi
    }
}

/// `pulses2bits`: cost in 1/8 bits of a pseudo-pulse count.
#[inline]
pub fn pulses2bits(band: usize, lm: i32, pulses: i32) -> i32 {
    if pulses == 0 {
        0
    } else {
        i32::from(cache_row(band, lm)[pulses as usize]) + 1
    }
}

/// `init_caps`: the maximum allocation per band.
pub fn init_caps(cap: &mut [i32; NB_EBANDS], lm: i32, c: i32) {
    for (i, cp) in cap.iter_mut().enumerate() {
        let n = (eb(i + 1) - eb(i)) << lm;
        *cp = (i32::from(CACHE_CAPS[NB_EBANDS * (2 * lm + c - 1) as usize + i]) + 64) * c * n >> 2;
    }
}

/// Results of [`compute_allocation`].
pub struct Allocation {
    pub coded_bands: usize,
    pub intensity: usize,
    pub dual_stereo: bool,
    pub balance: i32,
    pub pulses: [i32; NB_EBANDS],
    pub fine_quant: [i32; NB_EBANDS],
    pub fine_priority: [i32; NB_EBANDS],
}

/// `compute_allocation` (decoder side).
#[allow(clippy::too_many_arguments)]
pub fn compute_allocation(
    start: usize,
    end: usize,
    offsets: &[i32; NB_EBANDS],
    cap: &[i32; NB_EBANDS],
    alloc_trim: i32,
    total: i32,
    c: i32,
    lm: i32,
    ec: &mut RangeDecoder,
) -> Allocation {
    let mut total = total.max(0);
    let mut skip_start = start;
    let skip_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
    total -= skip_rsv;
    let mut intensity_rsv = 0;
    let mut dual_stereo_rsv = 0;
    if c == 2 {
        intensity_rsv = i32::from(LOG2_FRAC_TABLE[end - start]);
        if intensity_rsv > total {
            intensity_rsv = 0;
        } else {
            total -= intensity_rsv;
            dual_stereo_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
            total -= dual_stereo_rsv;
        }
    }
    let mut bits1 = [0i32; NB_EBANDS];
    let mut bits2 = [0i32; NB_EBANDS];
    let mut thresh = [0i32; NB_EBANDS];
    let mut trim_offset = [0i32; NB_EBANDS];
    for j in start..end {
        let w = eb(j + 1) - eb(j);
        thresh[j] = (c << BITRES).max((3 * w << lm << BITRES) >> 4);
        trim_offset[j] =
            c * w * (alloc_trim - 5 - lm) * (end as i32 - j as i32 - 1) * (1 << (lm + BITRES)) >> 6;
        if w << lm == 1 {
            trim_offset[j] -= c << BITRES;
        }
    }
    let mut lo = 1i32;
    let mut hi = NB_ALLOC_VECTORS as i32 - 1;
    loop {
        let mut done = false;
        let mut psum = 0i32;
        let mid = (lo + hi) >> 1;
        for j in (start..end).rev() {
            let n = eb(j + 1) - eb(j);
            let mut bitsj =
                c * n * i32::from(BAND_ALLOCATION[mid as usize * NB_EBANDS + j]) << lm >> 2;
            if bitsj > 0 {
                bitsj = (bitsj + trim_offset[j]).max(0);
            }
            bitsj += offsets[j];
            if bitsj >= thresh[j] || done {
                done = true;
                psum += bitsj.min(cap[j]);
            } else if bitsj >= c << BITRES {
                psum += c << BITRES;
            }
        }
        if psum > total {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
        if lo > hi {
            break;
        }
    }
    hi = lo;
    lo -= 1;
    for j in start..end {
        let n = eb(j + 1) - eb(j);
        let mut bits1j = c * n * i32::from(BAND_ALLOCATION[lo as usize * NB_EBANDS + j]) << lm >> 2;
        let mut bits2j = if hi as usize >= NB_ALLOC_VECTORS {
            cap[j]
        } else {
            c * n * i32::from(BAND_ALLOCATION[hi as usize * NB_EBANDS + j]) << lm >> 2
        };
        if bits1j > 0 {
            bits1j = (bits1j + trim_offset[j]).max(0);
        }
        if bits2j > 0 {
            bits2j = (bits2j + trim_offset[j]).max(0);
        }
        if lo > 0 {
            bits1j += offsets[j];
        }
        bits2j += offsets[j];
        if offsets[j] > 0 {
            skip_start = j;
        }
        bits2j = (bits2j - bits1j).max(0);
        bits1[j] = bits1j;
        bits2[j] = bits2j;
    }
    interp_bits2pulses(
        start,
        end,
        skip_start,
        &bits1,
        &bits2,
        &thresh,
        cap,
        total,
        skip_rsv,
        intensity_rsv,
        dual_stereo_rsv,
        c,
        lm,
        ec,
    )
}

#[allow(clippy::too_many_arguments)]
fn interp_bits2pulses(
    start: usize,
    end: usize,
    skip_start: usize,
    bits1: &[i32; NB_EBANDS],
    bits2: &[i32; NB_EBANDS],
    thresh: &[i32; NB_EBANDS],
    cap: &[i32; NB_EBANDS],
    mut total: i32,
    skip_rsv: i32,
    mut intensity_rsv: i32,
    mut dual_stereo_rsv: i32,
    c: i32,
    lm: i32,
    ec: &mut RangeDecoder,
) -> Allocation {
    let mut bits = [0i32; NB_EBANDS];
    let mut ebits = [0i32; NB_EBANDS];
    let mut fine_priority = [0i32; NB_EBANDS];
    let alloc_floor = c << BITRES;
    let stereo = i32::from(c > 1);
    let log_m = lm << BITRES;
    let mut lo = 0i32;
    let mut hi = 1i32 << ALLOC_STEPS;
    for _ in 0..ALLOC_STEPS {
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        let mut done = false;
        for j in (start..end).rev() {
            let tmp = bits1[j] + (mid * bits2[j] >> ALLOC_STEPS);
            if tmp >= thresh[j] || done {
                done = true;
                psum += tmp.min(cap[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let mut psum = 0i32;
    let mut done = false;
    for j in (start..end).rev() {
        let mut tmp = bits1[j] + (lo * bits2[j] >> ALLOC_STEPS);
        if tmp < thresh[j] && !done {
            tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
        } else {
            done = true;
        }
        tmp = tmp.min(cap[j]);
        bits[j] = tmp;
        psum += tmp;
    }
    // Decide which bands to skip, working backwards from the end.
    let mut coded_bands = end;
    loop {
        let j = coded_bands - 1;
        if j <= skip_start {
            total += skip_rsv;
            break;
        }
        let mut left = total - psum;
        let span = eb(coded_bands) - eb(start);
        let percoeff = left / span;
        left -= span * percoeff;
        let rem = (left - (eb(j) - eb(start))).max(0);
        let band_width = eb(coded_bands) - eb(j);
        let mut band_bits = bits[j] + percoeff * band_width + rem;
        if band_bits >= thresh[j].max(alloc_floor + (1 << BITRES)) {
            if ec.bit_logp(1) {
                break;
            }
            psum += 1 << BITRES;
            band_bits -= 1 << BITRES;
        }
        psum -= bits[j] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = i32::from(LOG2_FRAC_TABLE[j - start]);
        }
        psum += intensity_rsv;
        if band_bits >= alloc_floor {
            psum += alloc_floor;
            bits[j] = alloc_floor;
        } else {
            bits[j] = 0;
        }
        coded_bands -= 1;
    }
    let mut intensity = 0usize;
    if intensity_rsv > 0 {
        intensity = start + ec.uint((coded_bands + 1 - start) as u32) as usize;
    }
    if intensity <= start {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }
    let dual_stereo = dual_stereo_rsv > 0 && ec.bit_logp(1);

    // Allocate the remaining bits.
    let mut left = total - psum;
    let span = eb(coded_bands) - eb(start);
    let percoeff = left / span;
    left -= span * percoeff;
    for j in start..coded_bands {
        bits[j] += percoeff * (eb(j + 1) - eb(j));
    }
    for j in start..coded_bands {
        let tmp = left.min(eb(j + 1) - eb(j));
        bits[j] += tmp;
        left -= tmp;
    }
    let mut balance = 0i32;
    let mut j = start;
    while j < coded_bands {
        let n0 = eb(j + 1) - eb(j);
        let n = n0 << lm;
        bits[j] += balance;
        let mut excess;
        if n > 1 {
            excess = (bits[j] - cap[j]).max(0);
            bits[j] -= excess;
            let den = c * n + i32::from(c == 2 && n > 2 && !dual_stereo && j < intensity);
            let nclogn = den * (i32::from(LOG_N[j]) + log_m);
            let mut offset = (nclogn >> 1) - den * FINE_OFFSET;
            if n == 2 {
                offset += den << BITRES >> 2;
            }
            if bits[j] + offset < den * 2 << BITRES {
                offset += nclogn >> 2;
            } else if bits[j] + offset < den * 3 << BITRES {
                offset += nclogn >> 3;
            }
            ebits[j] = ((bits[j] + offset + (den << (BITRES - 1))) / (den << BITRES)).max(0);
            if c * ebits[j] > (bits[j] >> BITRES) {
                ebits[j] = bits[j] >> stereo >> BITRES;
            }
            ebits[j] = ebits[j].min(MAX_FINE_BITS);
            fine_priority[j] = i32::from(ebits[j] * (den << BITRES) >= bits[j] + offset);
            bits[j] -= c * ebits[j] << BITRES;
        } else {
            excess = (bits[j] - (c << BITRES)).max(0);
            bits[j] -= excess;
            ebits[j] = 0;
            fine_priority[j] = 1;
        }
        if excess > 0 {
            let extra_fine = (excess >> (stereo + BITRES)).min(MAX_FINE_BITS - ebits[j]);
            ebits[j] += extra_fine;
            let extra_bits = extra_fine * c << BITRES;
            fine_priority[j] = i32::from(extra_bits >= excess - balance);
            excess -= extra_bits;
        }
        balance = excess;
        j += 1;
    }
    // The skipped bands use all their bits for fine energy.
    while j < end {
        ebits[j] = bits[j] >> stereo >> BITRES;
        bits[j] = 0;
        fine_priority[j] = i32::from(ebits[j] < 1);
        j += 1;
    }
    Allocation {
        coded_bands,
        intensity,
        dual_stereo,
        balance,
        pulses: bits,
        fine_quant: ebits,
        fine_priority,
    }
}

//! The SILK layer (RFC 6716 §4.2): a fixed-point port of the reference decoder (`dec_API.c`,
//! `decode_frame.c`, `decode_indices.c`, `decode_pulses.c`, `decode_parameters.c`,
//! `decode_core.c`, `NLSF*.c`, `stereo_*.c` and the `resampler*.c` IIR/FIR up-sampler; see
//! `NOTICE`), including the RFC 8251 fixes. Being integer, it is bit-exact with the reference.
//!
//! Lost frames are concealed with a simple decaying extrapolation of the last excitation, not
//! the reference PLC.

// Ported fixed-/floating-point code keeps the reference implementation's expression and loop
// structure, which these lints would obscure.
#![allow(
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::needless_late_init
)]

mod tables;

use crate::range::RangeDecoder;
use tables::*;

const MAX_LPC_ORDER: usize = 16;
const MAX_FRAME_LENGTH: usize = 320;
const MAX_SUB_FRAME_LENGTH: usize = 80;
const LTP_ORDER: usize = 5;
const MAX_NB_SUBFR: usize = 4;
const TYPE_NO_VOICE_ACTIVITY: i32 = 0;
const TYPE_VOICED: i32 = 2;
const CODE_INDEPENDENTLY: i32 = 0;
const CODE_INDEPENDENTLY_NO_LTP_SCALING: i32 = 1;
const CODE_CONDITIONALLY: i32 = 2;
const N_LEVELS_QGAIN: i32 = 64;
const MAX_DELTA_GAIN_QUANT: i32 = 36;
const MIN_DELTA_GAIN_QUANT: i32 = -4;
const GAIN_OFFSET: i32 = 2090;
const GAIN_INV_SCALE_Q16: i32 = 1_907_825;
const QUANT_LEVEL_ADJUST_Q10: i32 = 80;
const NLSF_QUANT_LEVEL_ADJ_Q10: i32 = 102;
const MAX_LPC_STABILIZE_ITERATIONS: i32 = 16;
const INV_MAX_PRED_POWER_GAIN_Q30: i32 = 107_374;
const A_LIMIT_Q24: i32 = 16_773_022;
const BWE_AFTER_LOSS_Q16: i32 = 63570;
const SHELL_FRAME: usize = 16;
const MAX_PULSES: usize = 16;
const STEREO_INTERP_LEN_MS: i32 = 8;
const RESAMPLER_ORDER_FIR_12: usize = 8;
const RESAMPLER_MAX_BATCH_SIZE_IN: usize = 480;
const RAND_BUF_SIZE: usize = 128;
const V_PITCH_GAIN_START_MIN_Q14: i32 = 11469;
const V_PITCH_GAIN_START_MAX_Q14: i32 = 15565;

const LTP_SCALES_Q14: [i32; 3] = [15565, 12288, 8192];
const QUANT_OFFSETS_Q10: [[i32; 2]; 2] = [[100, 240], [32, 100]];
const STEREO_ONLY_CODE_MID_ICDF: [u8; 2] = [64, 0];
const LSB_ICDF: [u8; 2] = [120, 0];
const LTP_SCALE_ICDF: [u8; 3] = [128, 64, 0];
const TYPE_OFFSET_VAD_ICDF: [u8; 4] = [232, 158, 10, 0];
const TYPE_OFFSET_NO_VAD_ICDF: [u8; 2] = [230, 0];
const NLSF_INTERP_FACTOR_ICDF: [u8; 5] = [243, 221, 192, 181, 0];
const UNIFORM3_ICDF: [u8; 3] = [171, 85, 0];
const UNIFORM4_ICDF: [u8; 4] = [192, 128, 64, 0];
const UNIFORM5_ICDF: [u8; 5] = [205, 154, 102, 51, 0];
const UNIFORM6_ICDF: [u8; 6] = [213, 171, 128, 85, 43, 0];
const UNIFORM8_ICDF: [u8; 8] = [224, 192, 160, 128, 96, 64, 32, 0];
const NLSF_EXT_ICDF: [u8; 7] = [100, 40, 16, 7, 3, 1, 0];
const RESAMPLER_UP2_HQ_0: [i32; 3] = [1746, 14986, 39083 - 65536];
const RESAMPLER_UP2_HQ_1: [i32; 3] = [6854, 25769, 55542 - 65536];

// ---------------------------------------------------------------------------------------------
// Fixed-point helpers (SigProc_FIX.h / macros.h / Inlines.h semantics).

#[inline]
fn smulwb(a: i32, b: i32) -> i32 {
    ((i64::from(a) * i64::from(b as i16)) >> 16) as i32
}
#[inline]
fn smlawb(a: i32, b: i32, c: i32) -> i32 {
    a.wrapping_add(smulwb(b, c))
}
#[inline]
fn smulww(a: i32, b: i32) -> i32 {
    ((i64::from(a) * i64::from(b)) >> 16) as i32
}
#[inline]
fn smulbb(a: i32, b: i32) -> i32 {
    i32::from(a as i16) * i32::from(b as i16)
}
#[inline]
fn smmul(a: i32, b: i32) -> i32 {
    ((i64::from(a) * i64::from(b)) >> 32) as i32
}
#[inline]
fn rshift_round(a: i32, shift: u32) -> i32 {
    if shift == 1 {
        (a >> 1) + (a & 1)
    } else {
        ((a >> (shift - 1)) + 1) >> 1
    }
}
#[inline]
fn rshift_round64(a: i64, shift: u32) -> i64 {
    if shift == 1 {
        (a >> 1) + (a & 1)
    } else {
        ((a >> (shift - 1)) + 1) >> 1
    }
}
#[inline]
fn sat16(a: i32) -> i16 {
    a.clamp(-32768, 32767) as i16
}
#[inline]
fn clz32(x: i32) -> i32 {
    (x as u32).leading_zeros() as i32
}
#[inline]
fn lshift_sat32(a: i32, shift: i32) -> i32 {
    let lo = i32::MIN >> shift;
    let hi = i32::MAX >> shift;
    a.clamp(lo, hi) << shift
}

fn sqrt_approx(x: i32) -> i32 {
    if x <= 0 {
        return 0;
    }
    let lz = clz32(x);
    let frac_q7 = (x as u32).rotate_right((24 - lz).rem_euclid(32) as u32) as i32 & 0x7f;
    let mut y = if lz & 1 != 0 { 32768 } else { 46214 };
    y >>= lz >> 1;
    smlawb(y, y, smulbb(213, frac_q7))
}

fn div32_varq(a32: i32, b32: i32, qres: i32) -> i32 {
    if b32 == 0 {
        return 0;
    }
    let a_headrm = clz32(a32.wrapping_abs()) - 1;
    let a32_nrm = a32.wrapping_shl(a_headrm as u32);
    let b_headrm = clz32(b32.wrapping_abs()) - 1;
    let b32_nrm = b32.wrapping_shl(b_headrm as u32);
    let d = b32_nrm >> 16;
    if d == 0 {
        return 0;
    }
    let b32_inv = (i32::MAX >> 2) / d;
    let mut result = smulwb(a32_nrm, b32_inv);
    let a32_nrm = a32_nrm.wrapping_sub(smmul(b32_nrm, result).wrapping_shl(3));
    result = smlawb(result, a32_nrm, b32_inv);
    let lshift = 29 + a_headrm - b_headrm - qres;
    if lshift < 0 {
        lshift_sat32(result, -lshift)
    } else if lshift < 32 {
        result >> lshift
    } else {
        0
    }
}

fn inverse32_varq(b32: i32, qres: i32) -> i32 {
    if b32 == 0 {
        return 0;
    }
    let b_headrm = clz32(b32.wrapping_abs()) - 1;
    let b32_nrm = b32.wrapping_shl(b_headrm as u32);
    let d = b32_nrm >> 16;
    if d == 0 {
        return 0;
    }
    let b32_inv = (i32::MAX >> 2) / d;
    let mut result = b32_inv << 16;
    let err_q32 = ((1i32 << 29).wrapping_sub(smulwb(b32_nrm, b32_inv))).wrapping_shl(3);
    // silk_SMLAWW(result, err_Q32, b32_inv)
    result = result.wrapping_add(smulww(err_q32, b32_inv));
    let lshift = 61 - b_headrm - qres;
    if lshift <= 0 {
        lshift_sat32(result, -lshift)
    } else if lshift < 32 {
        result >> lshift
    } else {
        0
    }
}

fn log2lin(in_log_q7: i32) -> i32 {
    if in_log_q7 < 0 {
        return 0;
    }
    if in_log_q7 >= 31 << 7 {
        return i32::MAX;
    }
    let mut out = 1i32 << (in_log_q7 >> 7);
    let frac_q7 = in_log_q7 & 0x7f;
    let t = smlawb(frac_q7, smulbb(frac_q7, 128 - frac_q7), -174);
    if in_log_q7 < 2048 {
        out = out.wrapping_add(out.wrapping_mul(t) >> 7);
    } else {
        out = out.wrapping_add((out >> 7).wrapping_mul(t));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// NLSF codebooks.

struct NlsfCb {
    order: usize,
    quant_step_q16: i32,
    cb1_nlsf_q8: &'static [u8],
    cb1_icdf: &'static [u8],
    pred_q8: &'static [u8],
    ec_sel: &'static [u8],
    ec_icdf: &'static [u8],
    delta_min_q15: &'static [i16],
}

static NLSF_CB_NB_MB: NlsfCb = NlsfCb {
    order: 10,
    quant_step_q16: 11796,
    cb1_nlsf_q8: &NLSF_CB1_NB_MB_Q8,
    cb1_icdf: &NLSF_CB1_ICDF_NB_MB,
    pred_q8: &NLSF_PRED_NB_MB_Q8,
    ec_sel: &NLSF_CB2_SELECT_NB_MB,
    ec_icdf: &NLSF_CB2_ICDF_NB_MB,
    delta_min_q15: &NLSF_DELTA_MIN_NB_MB_Q15,
};

static NLSF_CB_WB: NlsfCb = NlsfCb {
    order: 16,
    quant_step_q16: 9830,
    cb1_nlsf_q8: &NLSF_CB1_WB_Q8,
    cb1_icdf: &NLSF_CB1_ICDF_WB,
    pred_q8: &NLSF_PRED_WB_Q8,
    ec_sel: &NLSF_CB2_SELECT_WB,
    ec_icdf: &NLSF_CB2_ICDF_WB,
    delta_min_q15: &NLSF_DELTA_MIN_WB_Q15,
};

fn nlsf_unpack(
    ec_ix: &mut [usize; MAX_LPC_ORDER],
    pred_q8: &mut [u8; MAX_LPC_ORDER],
    cb: &NlsfCb,
    cb1_index: usize,
) {
    let order = cb.order;
    let sel = &cb.ec_sel[cb1_index * order / 2..];
    for (k, i) in (0..order).step_by(2).enumerate() {
        let entry = sel[k];
        ec_ix[i] = usize::from((entry >> 1) & 7) * 9;
        pred_q8[i] = cb.pred_q8[i + usize::from(entry & 1) * (order - 1)];
        ec_ix[i + 1] = usize::from((entry >> 5) & 7) * 9;
        pred_q8[i + 1] = cb.pred_q8[i + usize::from((entry >> 4) & 1) * (order - 1) + 1];
    }
}

fn nlsf_stabilize(nlsf: &mut [i16], delta_min: &[i16], l: usize) {
    let dm = |i: usize| i32::from(delta_min[i]);
    for _ in 0..20 {
        let mut min_diff = i32::from(nlsf[0]) - dm(0);
        let mut ii = 0usize;
        for i in 1..l {
            let diff = i32::from(nlsf[i]) - (i32::from(nlsf[i - 1]) + dm(i));
            if diff < min_diff {
                min_diff = diff;
                ii = i;
            }
        }
        let diff = (1 << 15) - (i32::from(nlsf[l - 1]) + dm(l));
        if diff < min_diff {
            min_diff = diff;
            ii = l;
        }
        if min_diff >= 0 {
            return;
        }
        if ii == 0 {
            nlsf[0] = delta_min[0];
        } else if ii == l {
            nlsf[l - 1] = ((1 << 15) - dm(l)) as i16;
        } else {
            let mut min_center = 0i32;
            for k in 0..ii {
                min_center += dm(k);
            }
            min_center += dm(ii) >> 1;
            let mut max_center = 1i32 << 15;
            for k in (ii + 1..=l).rev() {
                max_center -= dm(k);
            }
            max_center -= dm(ii) >> 1;
            let c = rshift_round(i32::from(nlsf[ii - 1]) + i32::from(nlsf[ii]), 1);
            let center = if min_center > max_center {
                c.clamp(max_center, min_center)
            } else {
                c.clamp(min_center, max_center)
            } as i16;
            nlsf[ii - 1] = (i32::from(center) - (dm(ii) >> 1)) as i16;
            nlsf[ii] = (i32::from(nlsf[ii - 1]) + dm(ii)) as i16;
        }
    }
    // Fall back: sort, then enforce the minimum distances (RFC 8251 §7 saturating form).
    nlsf[..l].sort_unstable();
    nlsf[0] = nlsf[0].max(delta_min[0]);
    for i in 1..l {
        let lo = sat16(i32::from(nlsf[i - 1]) + dm(i));
        nlsf[i] = nlsf[i].max(lo);
    }
    nlsf[l - 1] = (i32::from(nlsf[l - 1]).min((1 << 15) - dm(l))) as i16;
    for i in (0..l - 1).rev() {
        nlsf[i] = (i32::from(nlsf[i]).min(i32::from(nlsf[i + 1]) - dm(i + 1))) as i16;
    }
}

fn nlsf_decode(
    nlsf_q15: &mut [i16; MAX_LPC_ORDER],
    indices: &[i8; MAX_LPC_ORDER + 1],
    cb: &NlsfCb,
) {
    let order = cb.order;
    let cb1 = usize::from(indices[0] as u8).min(31);
    for i in 0..order {
        nlsf_q15[i] = i16::from(cb.cb1_nlsf_q8[cb1 * order + i]) << 7;
    }
    let mut ec_ix = [0usize; MAX_LPC_ORDER];
    let mut pred_q8 = [0u8; MAX_LPC_ORDER];
    nlsf_unpack(&mut ec_ix, &mut pred_q8, cb, cb1);
    // Residual dequantisation.
    let mut res_q10 = [0i32; MAX_LPC_ORDER];
    let mut out_q10 = 0i32;
    for i in (0..order).rev() {
        let pred = smulbb(out_q10, i32::from(pred_q8[i])) >> 8;
        out_q10 = i32::from(indices[i + 1]) << 10;
        if out_q10 > 0 {
            out_q10 -= NLSF_QUANT_LEVEL_ADJ_Q10;
        } else if out_q10 < 0 {
            out_q10 += NLSF_QUANT_LEVEL_ADJ_Q10;
        }
        out_q10 = smlawb(pred, out_q10, cb.quant_step_q16);
        res_q10[i] = out_q10;
    }
    // Laroia weights of the first stage vector.
    let mut w = [0i32; MAX_LPC_ORDER];
    let nlv: [i32; MAX_LPC_ORDER] = std::array::from_fn(|i| i32::from(nlsf_q15[i]));
    let nl = |i: usize| nlv[i];
    let mut tmp1 = (1 << 17) / nl(0).max(1);
    let mut tmp2 = (1 << 17) / (nl(1) - nl(0)).max(1);
    w[0] = (tmp1 + tmp2).min(32767);
    let mut k = 1;
    while k < order - 1 {
        tmp1 = (1 << 17) / (nl(k + 1) - nl(k)).max(1);
        w[k] = (tmp1 + tmp2).min(32767);
        tmp2 = (1 << 17) / (nl(k + 2) - nl(k + 1)).max(1);
        w[k + 1] = (tmp1 + tmp2).min(32767);
        k += 2;
    }
    tmp1 = (1 << 17) / ((1 << 15) - nl(order - 1)).max(1);
    w[order - 1] = (tmp1 + tmp2).min(32767);
    for i in 0..order {
        let w_q9 = sqrt_approx(w[i] << 16).max(1);
        let v = nl(i) + (res_q10[i] << 14) / w_q9;
        nlsf_q15[i] = v.clamp(0, 32767) as i16;
    }
    nlsf_stabilize(&mut nlsf_q15[..], cb.delta_min_q15, order);
}

fn bwexpander(ar: &mut [i16], chirp_q16: i32) {
    let d = ar.len();
    let mut chirp = chirp_q16;
    let chirp_minus_one = chirp - 65536;
    for v in ar.iter_mut().take(d - 1) {
        *v = rshift_round(chirp.wrapping_mul(i32::from(*v)), 16) as i16;
        chirp += rshift_round(chirp.wrapping_mul(chirp_minus_one), 16);
    }
    ar[d - 1] = rshift_round(chirp.wrapping_mul(i32::from(ar[d - 1])), 16) as i16;
}

fn bwexpander_32(ar: &mut [i32], chirp_q16: i32) {
    let d = ar.len();
    let mut chirp = chirp_q16;
    let chirp_minus_one = chirp - 65536;
    for v in ar.iter_mut().take(d - 1) {
        *v = smulww(chirp, *v);
        chirp += rshift_round(chirp.wrapping_mul(chirp_minus_one), 16);
    }
    ar[d - 1] = smulww(chirp, ar[d - 1]);
}

/// `silk_LPC_inverse_pred_gain` with the RFC 8251 overflow check: 0 when unstable.
fn lpc_inverse_pred_gain(a_q12: &[i16]) -> i32 {
    const QA: i32 = 24;
    let order = a_q12.len();
    let mut a = [0i32; MAX_LPC_ORDER];
    let mut dc = 0i32;
    for k in 0..order {
        dc += i32::from(a_q12[k]);
        a[k] = i32::from(a_q12[k]) << (QA - 12);
    }
    if dc >= 4096 {
        return 0;
    }
    let mut inv_gain_q30 = 1i32 << 30;
    let mul_frac = |a: i32, b: i32, q: u32| rshift_round64(i64::from(a) * i64::from(b), q) as i32;
    for k in (1..order).rev() {
        if a[k] > A_LIMIT_Q24 || a[k] < -A_LIMIT_Q24 {
            return 0;
        }
        let rc_q31 = -(a[k] << (31 - QA));
        let rc_mult1_q30 = (1i32 << 30).wrapping_sub(smmul(rc_q31, rc_q31));
        let mult2q = 32 - clz32(rc_mult1_q30.wrapping_abs());
        let rc_mult2 = inverse32_varq(rc_mult1_q30, mult2q + 30);
        inv_gain_q30 = smmul(inv_gain_q30, rc_mult1_q30) << 2;
        let old = a;
        for n in 0..k {
            let tmp = old[n].saturating_sub(mul_frac(old[k - n - 1], rc_q31, 31));
            let t64 = rshift_round64(i64::from(tmp) * i64::from(rc_mult2), mult2q as u32);
            if t64 > i64::from(i32::MAX) || t64 < i64::from(i32::MIN) {
                return 0;
            }
            a[n] = t64 as i32;
        }
    }
    if a[0] > A_LIMIT_Q24 || a[0] < -A_LIMIT_Q24 {
        return 0;
    }
    let rc_q31 = -(a[0] << (31 - QA));
    let rc_mult1_q30 = (1i32 << 30).wrapping_sub(smmul(rc_q31, rc_q31));
    smmul(inv_gain_q30, rc_mult1_q30) << 2
}

fn nlsf2a(a_q12: &mut [i16], nlsf: &[i16]) {
    const ORDERING16: [usize; 16] = [0, 15, 8, 7, 4, 11, 12, 3, 2, 13, 10, 5, 6, 9, 14, 1];
    const ORDERING10: [usize; 10] = [0, 9, 6, 3, 4, 5, 8, 1, 2, 7];
    const QA: u32 = 16;
    let d = nlsf.len();
    let ordering: &[usize] = if d == 16 { &ORDERING16 } else { &ORDERING10 };
    let mut cos_lsf = [0i32; MAX_LPC_ORDER];
    for k in 0..d {
        let nk = i32::from(nlsf[k]).max(0);
        let f_int = (nk >> 8) as usize;
        let f_frac = nk - ((f_int as i32) << 8);
        let f_int = f_int.min(127);
        let cos_val = i32::from(LSF_COS_TAB_Q12[f_int]);
        let delta = i32::from(LSF_COS_TAB_Q12[f_int + 1]) - cos_val;
        cos_lsf[ordering[k]] = rshift_round((cos_val << 8) + delta * f_frac, 20 - QA);
    }
    let dd = d / 2;
    let find_poly = |out: &mut [i32; MAX_LPC_ORDER / 2 + 1], c: &[i32], off: usize| {
        out[0] = 1 << QA;
        out[1] = -c[off];
        for k in 1..dd {
            let ftmp = c[off + 2 * k];
            out[k + 1] = (out[k - 1] << 1)
                .wrapping_sub(rshift_round64(i64::from(ftmp) * i64::from(out[k]), QA) as i32);
            for n in (2..=k).rev() {
                out[n] = out[n].wrapping_add(
                    out[n - 2]
                        .wrapping_sub(
                            rshift_round64(i64::from(ftmp) * i64::from(out[n - 1]), QA) as i32
                        ),
                );
            }
            out[1] = out[1].wrapping_sub(ftmp);
        }
    };
    let mut p = [0i32; MAX_LPC_ORDER / 2 + 1];
    let mut q = [0i32; MAX_LPC_ORDER / 2 + 1];
    find_poly(&mut p, &cos_lsf, 0);
    find_poly(&mut q, &cos_lsf, 1);
    let mut a32 = [0i32; MAX_LPC_ORDER];
    for k in 0..dd {
        let ptmp = p[k + 1].wrapping_add(p[k]);
        let qtmp = q[k + 1].wrapping_sub(q[k]);
        a32[k] = (-qtmp).wrapping_sub(ptmp);
        a32[d - k - 1] = qtmp.wrapping_sub(ptmp);
    }
    let a32 = &mut a32[..d];
    let mut i = 0;
    while i < 10 {
        let mut maxabs = 0i32;
        let mut idx = 0usize;
        for (k, &v) in a32.iter().enumerate() {
            let absval = v.wrapping_abs();
            if absval > maxabs {
                maxabs = absval;
                idx = k;
            }
        }
        maxabs = rshift_round(maxabs, QA + 1 - 12);
        if maxabs > 32767 {
            maxabs = maxabs.min(163_838);
            let sc_q16 =
                65470 - ((maxabs - 32767) << 14) / ((maxabs.wrapping_mul(idx as i32 + 1)) >> 2);
            bwexpander_32(a32, sc_q16);
        } else {
            break;
        }
        i += 1;
    }
    if i == 10 {
        for k in 0..d {
            a_q12[k] = sat16(rshift_round(a32[k], QA + 1 - 12));
            a32[k] = i32::from(a_q12[k]) << (QA + 1 - 12);
        }
    } else {
        for k in 0..d {
            a_q12[k] = rshift_round(a32[k], QA + 1 - 12) as i16;
        }
    }
    for i in 0..MAX_LPC_STABILIZE_ITERATIONS {
        if lpc_inverse_pred_gain(&a_q12[..d]) < INV_MAX_PRED_POWER_GAIN_Q30 {
            bwexpander_32(a32, 65536 - (2 << i));
            for k in 0..d {
                a_q12[k] = rshift_round(a32[k], QA + 1 - 12) as i16;
            }
        } else {
            break;
        }
    }
}

fn lpc_analysis_filter(out: &mut [i16], input: &[i16], b: &[i16], len: usize, d: usize) {
    for ix in d..len {
        let mut acc = 0i32;
        for j in 0..d {
            acc = acc.wrapping_add(i32::from(input[ix - 1 - j]) * i32::from(b[j]));
        }
        let v = (i32::from(input[ix]) << 12).wrapping_sub(acc);
        out[ix] = sat16(rshift_round(v, 12));
    }
    out[..d].fill(0);
}

// ---------------------------------------------------------------------------------------------
// Resampler (IIR/FIR up-sampling to 48 kHz).

#[derive(Clone)]
struct Resampler {
    s_iir: [i32; 6],
    s_fir: [i16; RESAMPLER_ORDER_FIR_12],
    delay_buf: [i16; 48],
    fs_in_khz: usize,
    fs_out_khz: usize,
    input_delay: usize,
    batch_size: usize,
    inv_ratio_q16: i32,
}

impl Resampler {
    fn new(fs_in: i32, fs_out: i32) -> Self {
        let input_delay = match fs_in {
            8000 => [4, 0, 2, 0, 0],
            12000 => [0, 9, 4, 7, 4],
            _ => [0, 3, 12, 7, 7],
        }[match fs_out {
            8000 => 0,
            12000 => 1,
            16000 => 2,
            24000 => 3,
            _ => 4,
        }];
        let up2x = 1;
        let mut inv = ((fs_in << (14 + up2x)) / fs_out) << 2;
        while smulww(inv, fs_out) < (fs_in << up2x) {
            inv += 1;
        }
        Resampler {
            s_iir: [0; 6],
            s_fir: [0; RESAMPLER_ORDER_FIR_12],
            delay_buf: [0; 48],
            fs_in_khz: (fs_in / 1000) as usize,
            fs_out_khz: (fs_out / 1000) as usize,
            input_delay,
            batch_size: (fs_in / 1000) as usize * 10,
            inv_ratio_q16: inv,
        }
    }

    fn up2_hq(s: &mut [i32; 6], out: &mut [i16], input: &[i16]) {
        for (k, &x) in input.iter().enumerate() {
            let in32 = i32::from(x) << 10;
            let mut y = in32.wrapping_sub(s[0]);
            let mut xx = smulwb(y, RESAMPLER_UP2_HQ_0[0]);
            let mut o1 = s[0].wrapping_add(xx);
            s[0] = in32.wrapping_add(xx);
            y = o1.wrapping_sub(s[1]);
            xx = smulwb(y, RESAMPLER_UP2_HQ_0[1]);
            let mut o2 = s[1].wrapping_add(xx);
            s[1] = o1.wrapping_add(xx);
            y = o2.wrapping_sub(s[2]);
            xx = smlawb(y, y, RESAMPLER_UP2_HQ_0[2]);
            o1 = s[2].wrapping_add(xx);
            s[2] = o2.wrapping_add(xx);
            out[2 * k] = sat16(rshift_round(o1, 10));

            y = in32.wrapping_sub(s[3]);
            xx = smulwb(y, RESAMPLER_UP2_HQ_1[0]);
            o1 = s[3].wrapping_add(xx);
            s[3] = in32.wrapping_add(xx);
            y = o1.wrapping_sub(s[4]);
            xx = smulwb(y, RESAMPLER_UP2_HQ_1[1]);
            o2 = s[4].wrapping_add(xx);
            s[4] = o1.wrapping_add(xx);
            y = o2.wrapping_sub(s[5]);
            xx = smlawb(y, y, RESAMPLER_UP2_HQ_1[2]);
            o1 = s[5].wrapping_add(xx);
            s[5] = o2.wrapping_add(xx);
            out[2 * k + 1] = sat16(rshift_round(o1, 10));
        }
    }

    fn iir_fir(&mut self, out: &mut [i16], input: &[i16]) -> usize {
        let mut buf = [0i16; 2 * RESAMPLER_MAX_BATCH_SIZE_IN + RESAMPLER_ORDER_FIR_12];
        buf[..RESAMPLER_ORDER_FIR_12].copy_from_slice(&self.s_fir);
        let inc = self.inv_ratio_q16;
        let mut pos = 0usize;
        let mut in_off = 0usize;
        let mut in_len = input.len();
        let mut n_in;
        loop {
            n_in = in_len.min(self.batch_size);
            Self::up2_hq(
                &mut self.s_iir,
                &mut buf[RESAMPLER_ORDER_FIR_12..RESAMPLER_ORDER_FIR_12 + 2 * n_in],
                &input[in_off..in_off + n_in],
            );
            let max_index_q16 = (n_in as i32) << 17;
            let mut index_q16 = 0i32;
            while index_q16 < max_index_q16 {
                let ti = smulwb(index_q16 & 0xFFFF, 12) as usize;
                let bp = (index_q16 >> 16) as usize;
                let f = |r: usize, c: usize| i32::from(RESAMPLER_FRAC_FIR_12[r * 4 + c]);
                let b = |k: usize| i32::from(buf[bp + k]);
                let mut res = b(0) * f(ti, 0);
                res += b(1) * f(ti, 1);
                res += b(2) * f(ti, 2);
                res += b(3) * f(ti, 3);
                res += b(4) * f(11 - ti, 3);
                res += b(5) * f(11 - ti, 2);
                res += b(6) * f(11 - ti, 1);
                res += b(7) * f(11 - ti, 0);
                if pos < out.len() {
                    out[pos] = sat16(rshift_round(res, 15));
                }
                pos += 1;
                index_q16 += inc;
            }
            in_off += n_in;
            in_len -= n_in;
            if in_len > 0 {
                buf.copy_within(2 * n_in..2 * n_in + RESAMPLER_ORDER_FIR_12, 0);
            } else {
                break;
            }
        }
        self.s_fir
            .copy_from_slice(&buf[2 * n_in..2 * n_in + RESAMPLER_ORDER_FIR_12]);
        pos
    }

    /// `silk_resampler`: `input.len()` must be at least 1 ms.
    fn process(&mut self, out: &mut [i16], input: &[i16]) {
        let fs_in = self.fs_in_khz;
        let n_samples = fs_in - self.input_delay;
        self.delay_buf[self.input_delay..fs_in].copy_from_slice(&input[..n_samples]);
        let db = self.delay_buf;
        let fs_out = self.fs_out_khz;
        self.iir_fir(&mut out[..fs_out], &db[..fs_in]);
        let len = input.len();
        self.iir_fir(
            &mut out[fs_out..],
            &input[n_samples..n_samples + len - fs_in],
        );
        self.delay_buf[..self.input_delay].copy_from_slice(&input[len - self.input_delay..]);
    }
}

// ---------------------------------------------------------------------------------------------
// Channel state.

#[derive(Clone, Copy, Default)]
struct Indices {
    gains: [i8; MAX_NB_SUBFR],
    ltp_index: [i8; MAX_NB_SUBFR],
    nlsf: [i8; MAX_LPC_ORDER + 1],
    lag_index: i32,
    contour_index: i32,
    signal_type: i32,
    quant_offset_type: i32,
    nlsf_interp_coef_q2: i32,
    per_index: i32,
    ltp_scale_index: i32,
    seed: i32,
}

#[derive(Clone, Copy, Default)]
struct Control {
    pitch_l: [i32; MAX_NB_SUBFR],
    gains_q16: [i32; MAX_NB_SUBFR],
    pred_coef_q12: [[i16; MAX_LPC_ORDER]; 2],
    ltp_coef_q14: [i16; LTP_ORDER * MAX_NB_SUBFR],
    ltp_scale_q14: i32,
}

#[derive(Clone)]
struct Channel {
    prev_gain_q16: i32,
    exc_q14: [i32; MAX_FRAME_LENGTH],
    s_lpc_q14_buf: [i32; MAX_LPC_ORDER],
    out_buf: [i16; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
    lag_prev: i32,
    last_gain_index: i8,
    fs_khz: i32,
    fs_api_hz: i32,
    nb_subfr: usize,
    frame_length: usize,
    subfr_length: usize,
    ltp_mem_length: usize,
    lpc_order: usize,
    prev_nlsf_q15: [i16; MAX_LPC_ORDER],
    first_frame_after_reset: bool,
    pitch_lag_low_bits_icdf: &'static [u8],
    pitch_contour_icdf: &'static [u8],
    n_frames_decoded: usize,
    n_frames_per_packet: usize,
    ec_prev_signal_type: i32,
    ec_prev_lag_index: i32,
    vad_flags: [bool; 3],
    lbrr_flag: bool,
    lbrr_flags: [bool; 3],
    resampler: Option<Resampler>,
    nlsf_cb: &'static NlsfCb,
    indices: Indices,
    loss_cnt: i32,
    prev_signal_type: i32,
    plc: Plc,
}

impl Channel {
    fn new() -> Self {
        Channel {
            prev_gain_q16: 65536,
            exc_q14: [0; MAX_FRAME_LENGTH],
            s_lpc_q14_buf: [0; MAX_LPC_ORDER],
            out_buf: [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH],
            lag_prev: 0,
            last_gain_index: 0,
            fs_khz: 0,
            fs_api_hz: 0,
            nb_subfr: 0,
            frame_length: 0,
            subfr_length: 0,
            ltp_mem_length: 0,
            lpc_order: 0,
            prev_nlsf_q15: [0; MAX_LPC_ORDER],
            first_frame_after_reset: true,
            pitch_lag_low_bits_icdf: &UNIFORM8_ICDF,
            pitch_contour_icdf: &PITCH_CONTOUR_ICDF,
            n_frames_decoded: 0,
            n_frames_per_packet: 0,
            ec_prev_signal_type: 0,
            ec_prev_lag_index: 0,
            vad_flags: [false; 3],
            lbrr_flag: false,
            lbrr_flags: [false; 3],
            resampler: None,
            nlsf_cb: &NLSF_CB_NB_MB,
            indices: Indices::default(),
            loss_cnt: 0,
            prev_signal_type: 0,
            plc: Plc::default(),
        }
    }

    fn set_fs(&mut self, fs_khz: i32, fs_api_hz: i32) {
        self.subfr_length = 5 * fs_khz as usize;
        let frame_length = self.nb_subfr * self.subfr_length;
        if self.fs_khz != fs_khz || self.fs_api_hz != fs_api_hz {
            self.resampler = Some(Resampler::new(fs_khz * 1000, fs_api_hz));
            self.fs_api_hz = fs_api_hz;
        }
        if self.fs_khz != fs_khz || frame_length != self.frame_length {
            self.pitch_contour_icdf = match (fs_khz == 8, self.nb_subfr == MAX_NB_SUBFR) {
                (true, true) => &PITCH_CONTOUR_NB_ICDF,
                (true, false) => &PITCH_CONTOUR_10MS_NB_ICDF,
                (false, true) => &PITCH_CONTOUR_ICDF,
                (false, false) => &PITCH_CONTOUR_10MS_ICDF,
            };
            if self.fs_khz != fs_khz {
                self.ltp_mem_length = 20 * fs_khz as usize;
                if fs_khz == 8 || fs_khz == 12 {
                    self.lpc_order = 10;
                    self.nlsf_cb = &NLSF_CB_NB_MB;
                } else {
                    self.lpc_order = 16;
                    self.nlsf_cb = &NLSF_CB_WB;
                }
                self.pitch_lag_low_bits_icdf = match fs_khz {
                    16 => &UNIFORM8_ICDF,
                    12 => &UNIFORM6_ICDF,
                    _ => &UNIFORM4_ICDF,
                };
                self.first_frame_after_reset = true;
                self.lag_prev = 100;
                self.last_gain_index = 10;
                self.prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
                self.out_buf.fill(0);
                self.s_lpc_q14_buf.fill(0);
            }
            self.fs_khz = fs_khz;
            self.frame_length = frame_length;
        }
    }

    fn decode_indices(
        &mut self,
        ec: &mut RangeDecoder,
        frame_index: usize,
        decode_lbrr: bool,
        cond_coding: i32,
    ) {
        let ix = if decode_lbrr || self.vad_flags[frame_index] {
            ec.icdf(&TYPE_OFFSET_VAD_ICDF, 8) as i32 + 2
        } else {
            ec.icdf(&TYPE_OFFSET_NO_VAD_ICDF, 8) as i32
        };
        let ind = &mut self.indices;
        ind.signal_type = ix >> 1;
        ind.quant_offset_type = ix & 1;
        if cond_coding == CODE_CONDITIONALLY {
            ind.gains[0] = ec.icdf(&DELTA_GAIN_ICDF, 8) as i8;
        } else {
            let st = ind.signal_type as usize;
            let msb = (ec.icdf(&GAIN_ICDF[st * 8..st * 8 + 8], 8) as i32) << 3;
            ind.gains[0] = (msb + ec.icdf(&UNIFORM8_ICDF, 8) as i32) as i8;
        }
        for i in 1..self.nb_subfr {
            ind.gains[i] = ec.icdf(&DELTA_GAIN_ICDF, 8) as i8;
        }
        let cb = self.nlsf_cb;
        let st = (ind.signal_type >> 1) as usize;
        ind.nlsf[0] = ec.icdf(&cb.cb1_icdf[st * 32..], 8) as i8;
        let mut ec_ix = [0usize; MAX_LPC_ORDER];
        let mut pred_q8 = [0u8; MAX_LPC_ORDER];
        nlsf_unpack(&mut ec_ix, &mut pred_q8, cb, ind.nlsf[0] as usize);
        for i in 0..cb.order {
            let mut ix = ec.icdf(&cb.ec_icdf[ec_ix[i]..], 8) as i32;
            if ix == 0 {
                ix -= ec.icdf(&NLSF_EXT_ICDF, 8) as i32;
            } else if ix == 8 {
                ix += ec.icdf(&NLSF_EXT_ICDF, 8) as i32;
            }
            ind.nlsf[i + 1] = (ix - 4) as i8;
        }
        ind.nlsf_interp_coef_q2 = if self.nb_subfr == MAX_NB_SUBFR {
            ec.icdf(&NLSF_INTERP_FACTOR_ICDF, 8) as i32
        } else {
            4
        };
        if ind.signal_type == TYPE_VOICED {
            let mut absolute = true;
            if cond_coding == CODE_CONDITIONALLY && self.ec_prev_signal_type == TYPE_VOICED {
                let delta = ec.icdf(&PITCH_DELTA_ICDF, 8) as i32;
                if delta > 0 {
                    ind.lag_index = self.ec_prev_lag_index + delta - 9;
                    absolute = false;
                }
            }
            if absolute {
                ind.lag_index = ec.icdf(&PITCH_LAG_ICDF, 8) as i32 * (self.fs_khz >> 1);
                ind.lag_index += ec.icdf(self.pitch_lag_low_bits_icdf, 8) as i32;
            }
            self.ec_prev_lag_index = ind.lag_index;
            ind.contour_index = ec.icdf(self.pitch_contour_icdf, 8) as i32;
            ind.per_index = ec.icdf(&LTP_PER_INDEX_ICDF, 8) as i32;
            let tab: &[u8] = match ind.per_index {
                0 => &LTP_GAIN_ICDF_0,
                1 => &LTP_GAIN_ICDF_1,
                _ => &LTP_GAIN_ICDF_2,
            };
            for k in 0..self.nb_subfr {
                ind.ltp_index[k] = ec.icdf(tab, 8) as i8;
            }
            ind.ltp_scale_index = if cond_coding == CODE_INDEPENDENTLY {
                ec.icdf(&LTP_SCALE_ICDF, 8) as i32
            } else {
                0
            };
        }
        self.ec_prev_signal_type = ind.signal_type;
        ind.seed = ec.icdf(&UNIFORM4_ICDF, 8) as i32;
    }

    fn decode_parameters(&mut self, ctrl: &mut Control, cond_coding: i32) {
        // Gains.
        let mut prev = i32::from(self.last_gain_index);
        for k in 0..self.nb_subfr {
            let ind = i32::from(self.indices.gains[k]);
            if k == 0 && cond_coding != CODE_CONDITIONALLY {
                prev = ind.max(prev - 16);
            } else {
                let ind_tmp = ind + MIN_DELTA_GAIN_QUANT;
                let thr = 2 * MAX_DELTA_GAIN_QUANT - N_LEVELS_QGAIN + prev;
                if ind_tmp > thr {
                    prev += (ind_tmp << 1) - thr;
                } else {
                    prev += ind_tmp;
                }
            }
            prev = prev.clamp(0, N_LEVELS_QGAIN - 1);
            ctrl.gains_q16[k] = log2lin((smulwb(GAIN_INV_SCALE_Q16, prev) + GAIN_OFFSET).min(3967));
        }
        self.last_gain_index = prev as i8;

        // NLSFs.
        let order = self.lpc_order;
        let mut nlsf = [0i16; MAX_LPC_ORDER];
        nlsf_decode(&mut nlsf, &self.indices.nlsf, self.nlsf_cb);
        nlsf2a(&mut ctrl.pred_coef_q12[1][..order], &nlsf[..order]);
        if self.first_frame_after_reset {
            self.indices.nlsf_interp_coef_q2 = 4;
        }
        if self.indices.nlsf_interp_coef_q2 < 4 {
            let mut nlsf0 = [0i16; MAX_LPC_ORDER];
            for i in 0..order {
                let p = i32::from(self.prev_nlsf_q15[i]);
                nlsf0[i] = (p
                    + ((self.indices.nlsf_interp_coef_q2 * (i32::from(nlsf[i]) - p)) >> 2))
                    as i16;
            }
            nlsf2a(&mut ctrl.pred_coef_q12[0][..order], &nlsf0[..order]);
        } else {
            ctrl.pred_coef_q12[0] = ctrl.pred_coef_q12[1];
        }
        self.prev_nlsf_q15[..order].copy_from_slice(&nlsf[..order]);
        if self.loss_cnt != 0 {
            bwexpander(&mut ctrl.pred_coef_q12[0][..order], BWE_AFTER_LOSS_Q16);
            bwexpander(&mut ctrl.pred_coef_q12[1][..order], BWE_AFTER_LOSS_Q16);
        }

        if self.indices.signal_type == TYPE_VOICED {
            // Pitch lags.
            let fs = self.fs_khz;
            let (cb, size): (&[i8], usize) = match (fs == 8, self.nb_subfr == MAX_NB_SUBFR) {
                (true, true) => (&CB_LAGS_STAGE2, 11),
                (true, false) => (&CB_LAGS_STAGE2_10MS, 3),
                (false, true) => (&CB_LAGS_STAGE3, 34),
                (false, false) => (&CB_LAGS_STAGE3_10MS, 12),
            };
            let min_lag = 2 * fs;
            let max_lag = 18 * fs;
            let lag = min_lag + self.indices.lag_index;
            let contour = (self.indices.contour_index.max(0) as usize).min(size - 1);
            for k in 0..self.nb_subfr {
                ctrl.pitch_l[k] = (lag + i32::from(cb[k * size + contour])).clamp(min_lag, max_lag);
            }
            let vq: &[i8] = match self.indices.per_index {
                0 => &LTP_GAIN_VQ_0,
                1 => &LTP_GAIN_VQ_1,
                _ => &LTP_GAIN_VQ_2,
            };
            for k in 0..self.nb_subfr {
                let ix = self.indices.ltp_index[k] as usize;
                for i in 0..LTP_ORDER {
                    ctrl.ltp_coef_q14[k * LTP_ORDER + i] = i16::from(vq[ix * LTP_ORDER + i]) << 7;
                }
            }
            ctrl.ltp_scale_q14 = LTP_SCALES_Q14[self.indices.ltp_scale_index as usize];
        } else {
            ctrl.pitch_l = [0; MAX_NB_SUBFR];
            ctrl.ltp_coef_q14 = [0; LTP_ORDER * MAX_NB_SUBFR];
            self.indices.per_index = 0;
            ctrl.ltp_scale_q14 = 0;
        }
    }

    fn decode_core(&mut self, ctrl: &mut Control, xq: &mut [i16], pulses: &[i32]) {
        let mut s_ltp = [0i16; MAX_FRAME_LENGTH];
        let mut s_ltp_q15 = [0i32; 2 * MAX_FRAME_LENGTH];
        let mut res_q14 = [0i32; MAX_SUB_FRAME_LENGTH];
        let mut s_lpc_q14 = [0i32; MAX_SUB_FRAME_LENGTH + MAX_LPC_ORDER];
        let ind = self.indices;
        let offset_q10 =
            QUANT_OFFSETS_Q10[(ind.signal_type >> 1) as usize][ind.quant_offset_type as usize];
        let nlsf_interpolation = ind.nlsf_interp_coef_q2 < 4;
        let mut rand_seed = ind.seed;
        for i in 0..self.frame_length {
            rand_seed = 907_633_515i32.wrapping_add(rand_seed.wrapping_mul(196_314_165));
            let mut e = pulses[i] << 14;
            if e > 0 {
                e -= QUANT_LEVEL_ADJUST_Q10 << 4;
            } else if e < 0 {
                e += QUANT_LEVEL_ADJUST_Q10 << 4;
            }
            e += offset_q10 << 4;
            if rand_seed < 0 {
                e = -e;
            }
            self.exc_q14[i] = e;
            rand_seed = rand_seed.wrapping_add(pulses[i]);
        }
        s_lpc_q14[..MAX_LPC_ORDER].copy_from_slice(&self.s_lpc_q14_buf);
        let mut s_ltp_buf_idx = self.ltp_mem_length;
        let order = self.lpc_order;
        let sub = self.subfr_length;
        let mut lag = 0usize;
        for k in 0..self.nb_subfr {
            let a_q12 = ctrl.pred_coef_q12[k >> 1];
            let mut b_q14 = [0i16; LTP_ORDER];
            b_q14.copy_from_slice(&ctrl.ltp_coef_q14[k * LTP_ORDER..(k + 1) * LTP_ORDER]);
            let mut signal_type = ind.signal_type;
            let gain_q10 = ctrl.gains_q16[k] >> 6;
            let mut inv_gain_q31 = inverse32_varq(ctrl.gains_q16[k], 47);
            let gain_adj_q16 = if ctrl.gains_q16[k] != self.prev_gain_q16 {
                let g = div32_varq(self.prev_gain_q16, ctrl.gains_q16[k], 16);
                for v in s_lpc_q14.iter_mut().take(MAX_LPC_ORDER) {
                    *v = smulww(g, *v);
                }
                g
            } else {
                1 << 16
            };
            self.prev_gain_q16 = ctrl.gains_q16[k];
            if self.loss_cnt != 0
                && self.prev_signal_type == TYPE_VOICED
                && ind.signal_type != TYPE_VOICED
                && k < 2
            {
                b_q14 = [0; LTP_ORDER];
                b_q14[LTP_ORDER / 2] = 4096;
                signal_type = TYPE_VOICED;
                ctrl.pitch_l[k] = self.lag_prev;
            }
            if signal_type == TYPE_VOICED {
                lag = ctrl.pitch_l[k].max(0) as usize;
                if k == 0 || (k == 2 && nlsf_interpolation) {
                    let start_idx = self
                        .ltp_mem_length
                        .saturating_sub(lag + order + LTP_ORDER / 2)
                        .max(1);
                    if k == 2 {
                        let lm = self.ltp_mem_length;
                        self.out_buf[lm..lm + 2 * sub].copy_from_slice(&xq[..2 * sub]);
                    }
                    let len = self.ltp_mem_length - start_idx;
                    let src_off = start_idx + k * sub;
                    lpc_analysis_filter(
                        &mut s_ltp[start_idx..start_idx + len],
                        &self.out_buf[src_off..src_off + len],
                        &a_q12[..order],
                        len,
                        order,
                    );
                    if k == 0 {
                        inv_gain_q31 = smulwb(inv_gain_q31, ctrl.ltp_scale_q14) << 2;
                    }
                    for i in 0..(lag + LTP_ORDER / 2).min(self.ltp_mem_length) {
                        s_ltp_q15[s_ltp_buf_idx - i - 1] =
                            smulwb(inv_gain_q31, i32::from(s_ltp[self.ltp_mem_length - i - 1]));
                    }
                } else if gain_adj_q16 != 1 << 16 {
                    for i in 0..(lag + LTP_ORDER / 2).min(s_ltp_buf_idx) {
                        let p = s_ltp_buf_idx - i - 1;
                        s_ltp_q15[p] = smulww(gain_adj_q16, s_ltp_q15[p]);
                    }
                }
            }
            let exc = &self.exc_q14[k * sub..(k + 1) * sub];
            if signal_type == TYPE_VOICED {
                for i in 0..sub {
                    let base = s_ltp_buf_idx + LTP_ORDER / 2 - lag;
                    let mut pred = 2i32;
                    for (t, &bq) in b_q14.iter().enumerate() {
                        pred = smlawb(pred, s_ltp_q15[base - t], i32::from(bq));
                    }
                    res_q14[i] = exc[i].wrapping_add(pred << 1);
                    s_ltp_q15[s_ltp_buf_idx] = res_q14[i] << 1;
                    s_ltp_buf_idx += 1;
                }
            } else {
                res_q14[..sub].copy_from_slice(exc);
            }
            for i in 0..sub {
                let mut pred = (order >> 1) as i32;
                for j in 0..order {
                    pred = smlawb(
                        pred,
                        s_lpc_q14[MAX_LPC_ORDER + i - 1 - j],
                        i32::from(a_q12[j]),
                    );
                }
                s_lpc_q14[MAX_LPC_ORDER + i] = res_q14[i].wrapping_add(pred.wrapping_shl(4));
                xq[k * sub + i] = sat16(rshift_round(
                    smulww(s_lpc_q14[MAX_LPC_ORDER + i], gain_q10),
                    8,
                ));
            }
            s_lpc_q14.copy_within(sub..sub + MAX_LPC_ORDER, 0);
        }
        self.s_lpc_q14_buf
            .copy_from_slice(&s_lpc_q14[..MAX_LPC_ORDER]);
    }

    /// `silk_PLC_update`: remember the parameters of a good frame for concealment.
    fn plc_update(&mut self, ctrl: &Control) {
        let plc = &mut self.plc;
        let nb = self.nb_subfr;
        let mut ltp_gain_q14 = 0i32;
        if self.indices.signal_type == TYPE_VOICED {
            let mut j = 0;
            while j * self.subfr_length < ctrl.pitch_l[nb - 1].max(0) as usize {
                if j == nb {
                    break;
                }
                let base = (nb - 1 - j) * LTP_ORDER;
                let tmp: i32 = ctrl.ltp_coef_q14[base..base + LTP_ORDER]
                    .iter()
                    .map(|&v| i32::from(v))
                    .sum();
                if tmp > ltp_gain_q14 {
                    ltp_gain_q14 = tmp;
                    plc.ltp_coef_q14
                        .copy_from_slice(&ctrl.ltp_coef_q14[base..base + LTP_ORDER]);
                    plc.pitch_l_q8 = ctrl.pitch_l[nb - 1 - j] << 8;
                }
                j += 1;
            }
            plc.ltp_coef_q14 = [0; LTP_ORDER];
            plc.ltp_coef_q14[LTP_ORDER / 2] = ltp_gain_q14 as i16;
            if ltp_gain_q14 < V_PITCH_GAIN_START_MIN_Q14 {
                let scale_q10 = (V_PITCH_GAIN_START_MIN_Q14 << 10) / ltp_gain_q14.max(1);
                for v in plc.ltp_coef_q14.iter_mut() {
                    *v = (smulbb(i32::from(*v), scale_q10) >> 10) as i16;
                }
            } else if ltp_gain_q14 > V_PITCH_GAIN_START_MAX_Q14 {
                let scale_q14 = (V_PITCH_GAIN_START_MAX_Q14 << 14) / ltp_gain_q14.max(1);
                for v in plc.ltp_coef_q14.iter_mut() {
                    *v = (smulbb(i32::from(*v), scale_q14) >> 14) as i16;
                }
            }
        } else {
            plc.pitch_l_q8 = smulbb(self.fs_khz, 18) << 8;
            plc.ltp_coef_q14 = [0; LTP_ORDER];
        }
        let order = self.lpc_order;
        plc.prev_lpc_q12[..order].copy_from_slice(&ctrl.pred_coef_q12[1][..order]);
        plc.prev_ltp_scale_q14 = ctrl.ltp_scale_q14;
        plc.prev_gain_q16 = [ctrl.gains_q16[nb - 2], ctrl.gains_q16[nb - 1]];
        plc.subfr_length = self.subfr_length;
        plc.nb_subfr = nb;
    }

    /// `silk_PLC_conceal`: extrapolate a lost frame from the last good one.
    fn plc_conceal(&mut self, ctrl: &mut Control, frame: &mut [i16]) {
        let order = self.lpc_order;
        let ltp_mem = self.ltp_mem_length;
        let prev_gain_q10 = [
            self.plc.prev_gain_q16[0] >> 6,
            self.plc.prev_gain_q16[1] >> 6,
        ];
        if self.first_frame_after_reset {
            self.plc.prev_lpc_q12 = [0; MAX_LPC_ORDER];
        }
        let psub = self.plc.subfr_length;
        let pnb = self.plc.nb_subfr.max(2);
        let mut exc_buf = [0i16; 2 * MAX_SUB_FRAME_LENGTH];
        for k in 0..2 {
            for i in 0..psub {
                let e = self.exc_q14[(i + (k + pnb - 2) * psub).min(MAX_FRAME_LENGTH - 1)];
                exc_buf[k * psub + i] = sat16(smulww(e, prev_gain_q10[k]) >> 8);
            }
        }
        let (energy1, shift1) = sum_sqr_shift(&exc_buf[..psub]);
        let (energy2, shift2) = sum_sqr_shift(&exc_buf[psub..2 * psub]);
        let rand_start = if (energy1 >> shift2) < (energy2 >> shift1) {
            ((pnb - 1) * psub).saturating_sub(RAND_BUF_SIZE)
        } else {
            (pnb * psub).saturating_sub(RAND_BUF_SIZE)
        }
        .min(MAX_FRAME_LENGTH - RAND_BUF_SIZE);
        let mut b_q14 = self.plc.ltp_coef_q14;
        let mut rand_scale_q14 = self.plc.rand_scale_q14;
        let att = (self.loss_cnt.max(0) as usize).min(1);
        let harm_gain_q15 = [32440, 31130][att];
        let mut rand_gain_q15 = if self.prev_signal_type == TYPE_VOICED {
            [31130, 26214][att]
        } else {
            [32440, 29491][att]
        };
        bwexpander(&mut self.plc.prev_lpc_q12[..order], 64881);
        let a_q12 = self.plc.prev_lpc_q12;
        if self.loss_cnt == 0 {
            rand_scale_q14 = 1 << 14;
            if self.prev_signal_type == TYPE_VOICED {
                for &b in &b_q14 {
                    rand_scale_q14 = rand_scale_q14.wrapping_sub(b);
                }
                rand_scale_q14 = rand_scale_q14.max(3277);
                rand_scale_q14 =
                    (smulbb(i32::from(rand_scale_q14), self.plc.prev_ltp_scale_q14) >> 14) as i16;
            } else {
                let inv_gain_q30 = lpc_inverse_pred_gain(&self.plc.prev_lpc_q12[..order]);
                let mut down = ((1i32 << 30) >> 3).min(inv_gain_q30);
                down = ((1i32 << 30) >> 8).max(down);
                down <<= 3;
                rand_gain_q15 = smulwb(down, rand_gain_q15) >> 14;
            }
        }
        let mut rand_seed = self.plc.rand_seed;
        let mut lag =
            rshift_round(self.plc.pitch_l_q8, 8).clamp(1, (18 * self.fs_khz).max(1)) as usize;
        let mut s_ltp_buf_idx = ltp_mem;
        let mut s_ltp = [0i16; MAX_FRAME_LENGTH];
        let mut s_ltp_q14 = [0i32; 2 * MAX_FRAME_LENGTH + LTP_ORDER];
        let idx = ltp_mem.saturating_sub(lag + order + LTP_ORDER / 2).max(1);
        lpc_analysis_filter(
            &mut s_ltp[idx..ltp_mem],
            &self.out_buf[idx..ltp_mem],
            &a_q12[..order],
            ltp_mem - idx,
            order,
        );
        let inv_gain_q30 = inverse32_varq(self.plc.prev_gain_q16[1].max(1), 46).min(i32::MAX >> 1);
        for i in (idx + order).min(ltp_mem)..ltp_mem {
            s_ltp_q14[i] = smulwb(inv_gain_q30, i32::from(s_ltp[i]));
        }
        for _ in 0..self.nb_subfr {
            for _ in 0..self.subfr_length {
                let base = s_ltp_buf_idx + LTP_ORDER / 2 - lag.min(s_ltp_buf_idx);
                let mut pred = 2i32;
                for (t, &bq) in b_q14.iter().enumerate() {
                    pred = smlawb(pred, s_ltp_q14[base.saturating_sub(t)], i32::from(bq));
                }
                rand_seed = 907_633_515i32.wrapping_add(rand_seed.wrapping_mul(196_314_165));
                let ri = ((rand_seed >> 25) & (RAND_BUF_SIZE as i32 - 1)) as usize;
                s_ltp_q14[s_ltp_buf_idx] = smlawb(
                    pred,
                    self.exc_q14[rand_start + ri],
                    i32::from(rand_scale_q14),
                )
                .wrapping_shl(2);
                s_ltp_buf_idx += 1;
            }
            for b in b_q14.iter_mut() {
                *b = (smulbb(harm_gain_q15, i32::from(*b)) >> 15) as i16;
            }
            rand_scale_q14 = (smulbb(i32::from(rand_scale_q14), rand_gain_q15) >> 15) as i16;
            self.plc.pitch_l_q8 = smlawb(self.plc.pitch_l_q8, self.plc.pitch_l_q8, 655);
            self.plc.pitch_l_q8 = self.plc.pitch_l_q8.min(smulbb(18, self.fs_khz) << 8);
            lag = rshift_round(self.plc.pitch_l_q8, 8).max(1) as usize;
        }
        // LPC synthesis, in place over the LTP output.
        let off = ltp_mem - MAX_LPC_ORDER;
        s_ltp_q14[off..off + MAX_LPC_ORDER].copy_from_slice(&self.s_lpc_q14_buf);
        for i in 0..self.frame_length {
            let mut pred = (order >> 1) as i32;
            for j in 0..order {
                pred = smlawb(
                    pred,
                    s_ltp_q14[off + MAX_LPC_ORDER + i - 1 - j],
                    i32::from(a_q12[j]),
                );
            }
            let k = off + MAX_LPC_ORDER + i;
            s_ltp_q14[k] = s_ltp_q14[k].wrapping_add(pred.wrapping_shl(4));
            frame[i] = sat16(rshift_round(smulww(s_ltp_q14[k], prev_gain_q10[1]), 8));
        }
        let fl = self.frame_length;
        self.s_lpc_q14_buf
            .copy_from_slice(&s_ltp_q14[off + fl..off + fl + MAX_LPC_ORDER]);
        self.plc.rand_seed = rand_seed;
        self.plc.rand_scale_q14 = rand_scale_q14;
        ctrl.pitch_l = [lag as i32; MAX_NB_SUBFR];
    }

    /// `silk_PLC_glue_frames`: fade a good frame in after concealed ones.
    fn plc_glue_frames(&mut self, frame: &mut [i16]) {
        let plc = &mut self.plc;
        if self.loss_cnt != 0 {
            let (e, s) = sum_sqr_shift(frame);
            plc.conc_energy = e;
            plc.conc_energy_shift = s;
            plc.last_frame_lost = true;
        } else {
            if plc.last_frame_lost {
                let (mut energy, energy_shift) = sum_sqr_shift(frame);
                if energy_shift > plc.conc_energy_shift {
                    plc.conc_energy >>= energy_shift - plc.conc_energy_shift;
                } else if energy_shift < plc.conc_energy_shift {
                    energy >>= plc.conc_energy_shift - energy_shift;
                }
                if energy > plc.conc_energy {
                    let lz = clz32(plc.conc_energy) - 1;
                    plc.conc_energy = plc.conc_energy.wrapping_shl(lz.clamp(0, 31) as u32);
                    energy >>= (24 - lz).clamp(0, 31);
                    let frac_q24 = plc.conc_energy / energy.max(1);
                    let mut gain_q16 = sqrt_approx(frac_q24) << 4;
                    let slope_q16 = (((1 << 16) - gain_q16) / frame.len().max(1) as i32) << 2;
                    for v in frame.iter_mut() {
                        *v = smulwb(gain_q16, i32::from(*v)) as i16;
                        gain_q16 += slope_q16;
                        if gain_q16 > 1 << 16 {
                            break;
                        }
                    }
                }
            }
            plc.last_frame_lost = false;
        }
    }

    /// `silk_decode_frame`.
    fn decode_frame(
        &mut self,
        ec: Option<&mut RangeDecoder>,
        out: &mut [i16],
        lbrr: bool,
        cond_coding: i32,
    ) -> usize {
        let l = self.frame_length;
        if self.plc.fs_khz != self.fs_khz {
            self.plc = Plc::reset(self.frame_length);
            self.plc.fs_khz = self.fs_khz;
        }
        let mut ctrl = Control::default();
        match ec {
            Some(ec) => {
                self.decode_indices(ec, self.n_frames_decoded, lbrr, cond_coding);
                let mut pulses = [0i32; MAX_FRAME_LENGTH];
                decode_pulses(
                    ec,
                    &mut pulses,
                    self.indices.signal_type,
                    self.indices.quant_offset_type,
                    l,
                );
                self.decode_parameters(&mut ctrl, cond_coding);
                self.decode_core(&mut ctrl, out, &pulses);
                self.prev_signal_type = self.indices.signal_type;
                self.plc_update(&ctrl);
                self.loss_cnt = 0;
                self.first_frame_after_reset = false;
            }
            None => {
                self.plc_conceal(&mut ctrl, out);
                self.loss_cnt += 1;
            }
        }
        let mv = self.ltp_mem_length - l;
        self.out_buf.copy_within(l..l + mv, 0);
        self.out_buf[mv..mv + l].copy_from_slice(&out[..l]);
        self.plc_glue_frames(&mut out[..l]);
        self.lag_prev = ctrl.pitch_l[self.nb_subfr - 1];
        l
    }
}

/// `silk_sum_sqr_shift`: energy of `x` and the right shift applied to keep it in 31 bits.
fn sum_sqr_shift(x: &[i16]) -> (i32, i32) {
    let len = x.len();
    let mut nrg: i32 = 0;
    let mut shft = 0i32;
    let sq = |v: i16| i32::from(v) * i32::from(v);
    let mut i = 0;
    while i + 1 < len {
        nrg = nrg.wrapping_add(sq(x[i])).wrapping_add(sq(x[i + 1]));
        i += 2;
        if nrg < 0 {
            nrg = ((nrg as u32) >> 2) as i32;
            shft = 2;
            break;
        }
    }
    while i + 1 < len {
        let t = sq(x[i]).wrapping_add(sq(x[i + 1]));
        nrg = (nrg as u32).wrapping_add((t as u32) >> shft) as i32;
        if nrg < 0 {
            nrg = ((nrg as u32) >> 2) as i32;
            shft += 2;
        }
        i += 2;
    }
    if i + 1 == len {
        nrg = (nrg as u32).wrapping_add((sq(x[i]) as u32) >> shft) as i32;
    }
    if nrg as u32 & 0xC000_0000 != 0 {
        nrg = ((nrg as u32) >> 2) as i32;
        shft += 2;
    }
    (nrg, shft)
}

/// Packet loss concealment state (`silk_PLC_struct`).
#[derive(Clone, Copy)]
struct Plc {
    pitch_l_q8: i32,
    ltp_coef_q14: [i16; LTP_ORDER],
    prev_lpc_q12: [i16; MAX_LPC_ORDER],
    last_frame_lost: bool,
    rand_seed: i32,
    rand_scale_q14: i16,
    conc_energy: i32,
    conc_energy_shift: i32,
    prev_ltp_scale_q14: i32,
    prev_gain_q16: [i32; 2],
    fs_khz: i32,
    nb_subfr: usize,
    subfr_length: usize,
}

impl Default for Plc {
    fn default() -> Self {
        Plc::reset(0)
    }
}

impl Plc {
    fn reset(frame_length: usize) -> Self {
        Plc {
            pitch_l_q8: (frame_length as i32) << 7,
            ltp_coef_q14: [0; LTP_ORDER],
            prev_lpc_q12: [0; MAX_LPC_ORDER],
            last_frame_lost: false,
            rand_seed: 0,
            rand_scale_q14: 0,
            conc_energy: 0,
            conc_energy_shift: 0,
            prev_ltp_scale_q14: 0,
            prev_gain_q16: [65536; 2],
            fs_khz: 0,
            nb_subfr: 2,
            subfr_length: 20,
        }
    }
}

fn decode_pulses(
    ec: &mut RangeDecoder,
    pulses: &mut [i32; MAX_FRAME_LENGTH],
    signal_type: i32,
    quant_offset_type: i32,
    frame_length: usize,
) {
    let rl = (signal_type >> 1) as usize * 9;
    let rate_level = ec.icdf(&RATE_LEVELS_ICDF[rl..rl + 9], 8);
    let mut iter = frame_length / SHELL_FRAME;
    if iter * SHELL_FRAME < frame_length {
        iter += 1;
    }
    let mut sum_pulses = [0i32; 20];
    let mut n_lshifts = [0i32; 20];
    let cdf = &PULSES_PER_BLOCK_ICDF[rate_level.min(9) * 18..];
    for i in 0..iter {
        sum_pulses[i] = ec.icdf(&cdf[..18], 8) as i32;
        while sum_pulses[i] == MAX_PULSES as i32 + 1 {
            n_lshifts[i] += 1;
            let off = 9 * 18 + usize::from(n_lshifts[i] == 10);
            sum_pulses[i] = ec.icdf(&PULSES_PER_BLOCK_ICDF[off..9 * 18 + 18], 8) as i32;
            if n_lshifts[i] > 10 {
                break;
            }
        }
    }
    for i in 0..iter {
        let blk = &mut pulses[i * SHELL_FRAME..(i + 1) * SHELL_FRAME];
        if sum_pulses[i] > 0 {
            shell_decoder(blk, ec, sum_pulses[i].min(MAX_PULSES as i32) as usize);
        } else {
            blk.fill(0);
        }
    }
    for i in 0..iter {
        if n_lshifts[i] > 0 {
            let nls = n_lshifts[i];
            for v in pulses[i * SHELL_FRAME..(i + 1) * SHELL_FRAME].iter_mut() {
                let mut q = *v;
                for _ in 0..nls {
                    q = (q << 1) + ec.icdf(&LSB_ICDF, 8) as i32;
                }
                *v = q;
            }
            sum_pulses[i] |= nls << 5;
        }
    }
    // Signs.
    let base = 7 * (quant_offset_type + (signal_type << 1)) as usize;
    let icdf_ptr = &SIGN_ICDF[base..base + 7];
    let blocks = (frame_length + SHELL_FRAME / 2) >> 4;
    for i in 0..blocks {
        let p = sum_pulses[i];
        if p > 0 {
            let icdf = [icdf_ptr[(p & 0x1f).min(6) as usize], 0];
            for v in pulses[i * SHELL_FRAME..(i + 1) * SHELL_FRAME].iter_mut() {
                if *v > 0 {
                    let s = ec.icdf(&icdf, 8) as i32;
                    *v *= (s << 1) - 1;
                }
            }
        }
    }
}

fn shell_decoder(out: &mut [i32], ec: &mut RangeDecoder, pulses4: usize) {
    let split = |ec: &mut RangeDecoder, p: usize, table: &[u8]| -> (usize, usize) {
        if p > 0 {
            let off = usize::from(SHELL_CODE_TABLE_OFFSETS[p]);
            let a = ec.icdf(&table[off..], 8).min(p);
            (a, p - a)
        } else {
            (0, 0)
        }
    };
    let mut p3 = [0usize; 2];
    let mut p2 = [0usize; 4];
    let mut p1 = [0usize; 8];
    let mut p0 = [0usize; 16];
    (p3[0], p3[1]) = split(ec, pulses4, &SHELL_CODE_TABLE3);
    (p2[0], p2[1]) = split(ec, p3[0], &SHELL_CODE_TABLE2);
    (p1[0], p1[1]) = split(ec, p2[0], &SHELL_CODE_TABLE1);
    (p0[0], p0[1]) = split(ec, p1[0], &SHELL_CODE_TABLE0);
    (p0[2], p0[3]) = split(ec, p1[1], &SHELL_CODE_TABLE0);
    (p1[2], p1[3]) = split(ec, p2[1], &SHELL_CODE_TABLE1);
    (p0[4], p0[5]) = split(ec, p1[2], &SHELL_CODE_TABLE0);
    (p0[6], p0[7]) = split(ec, p1[3], &SHELL_CODE_TABLE0);
    (p2[2], p2[3]) = split(ec, p3[1], &SHELL_CODE_TABLE2);
    (p1[4], p1[5]) = split(ec, p2[2], &SHELL_CODE_TABLE1);
    (p0[8], p0[9]) = split(ec, p1[4], &SHELL_CODE_TABLE0);
    (p0[10], p0[11]) = split(ec, p1[5], &SHELL_CODE_TABLE0);
    (p1[6], p1[7]) = split(ec, p2[3], &SHELL_CODE_TABLE1);
    (p0[12], p0[13]) = split(ec, p1[6], &SHELL_CODE_TABLE0);
    (p0[14], p0[15]) = split(ec, p1[7], &SHELL_CODE_TABLE0);
    for (o, &v) in out.iter_mut().zip(p0.iter()) {
        *o = v as i32;
    }
}

fn stereo_decode_pred(ec: &mut RangeDecoder) -> [i32; 2] {
    let n = ec.icdf(&STEREO_PRED_JOINT_ICDF, 8) as i32;
    let mut ix = [[0i32; 3]; 2];
    ix[0][2] = n / 5;
    ix[1][2] = n - 5 * ix[0][2];
    for row in ix.iter_mut() {
        row[0] = ec.icdf(&UNIFORM3_ICDF, 8) as i32;
        row[1] = ec.icdf(&UNIFORM5_ICDF, 8) as i32;
    }
    let mut pred = [0i32; 2];
    for (p, row) in pred.iter_mut().zip(ix.iter_mut()) {
        row[0] += 3 * row[2];
        let i0 = (row[0] as usize).min(14);
        let low = i32::from(STEREO_PRED_QUANT_Q13[i0]);
        let step = smulwb(i32::from(STEREO_PRED_QUANT_Q13[i0 + 1]) - low, 6554);
        *p = low + step * (2 * row[1] + 1);
    }
    pred[0] -= pred[1];
    pred
}

#[derive(Clone, Copy, Default)]
struct StereoState {
    pred_prev_q13: [i32; 2],
    s_mid: [i16; 2],
    s_side: [i16; 2],
}

fn stereo_ms_to_lr(
    st: &mut StereoState,
    x1: &mut [i16],
    x2: &mut [i16],
    pred_q13: [i32; 2],
    fs_khz: i32,
    frame_length: usize,
) {
    x1[..2].copy_from_slice(&st.s_mid);
    x2[..2].copy_from_slice(&st.s_side);
    st.s_mid
        .copy_from_slice(&x1[frame_length..frame_length + 2]);
    st.s_side
        .copy_from_slice(&x2[frame_length..frame_length + 2]);
    let mut pred0 = st.pred_prev_q13[0];
    let mut pred1 = st.pred_prev_q13[1];
    let interp = (STEREO_INTERP_LEN_MS * fs_khz) as usize;
    let denom_q16 = (1 << 16) / (STEREO_INTERP_LEN_MS * fs_khz);
    let delta0 = rshift_round(smulbb(pred_q13[0] - st.pred_prev_q13[0], denom_q16), 16);
    let delta1 = rshift_round(smulbb(pred_q13[1] - st.pred_prev_q13[1], denom_q16), 16);
    for n in 0..frame_length {
        if n < interp {
            pred0 += delta0;
            pred1 += delta1;
        } else if n == interp {
            pred0 = pred_q13[0];
            pred1 = pred_q13[1];
        }
        let x1n = |k: usize| i32::from(x1[k]);
        let mut sum = (x1n(n) + x1n(n + 2) + (x1n(n + 1) << 1)) << 9;
        sum = smlawb(i32::from(x2[n + 1]) << 8, sum, pred0);
        sum = smlawb(sum, x1n(n + 1) << 11, pred1);
        x2[n + 1] = sat16(rshift_round(sum, 8));
    }
    st.pred_prev_q13 = pred_q13;
    for n in 0..frame_length {
        let sum = i32::from(x1[n + 1]) + i32::from(x2[n + 1]);
        let diff = i32::from(x1[n + 1]) - i32::from(x2[n + 1]);
        x1[n + 1] = sat16(sum);
        x2[n + 1] = sat16(diff);
    }
}

/// Per-call parameters of [`SilkDecoder::decode`] (`silk_DecControlStruct`).
#[derive(Clone, Copy, Debug)]
pub struct SilkControl {
    /// Output channels (1 or 2).
    pub channels_api: usize,
    /// Channels coded in the stream.
    pub channels_internal: usize,
    /// Internal sample rate: 8000, 12000 or 16000.
    pub internal_rate: i32,
    /// Packet duration in ms (10, 20, 40 or 60).
    pub payload_ms: i32,
}

/// The SILK decoder for one Opus stream (`silk_decoder`), output at 48 kHz.
pub struct SilkDecoder {
    ch: [Channel; 2],
    stereo: StereoState,
    n_channels_api: usize,
    n_channels_internal: usize,
    prev_decode_only_middle: bool,
    tmp: [[i16; MAX_FRAME_LENGTH + 2]; 2],
    resampled: [i16; 960],
}

impl Default for SilkDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SilkDecoder {
    pub fn new() -> Self {
        SilkDecoder {
            ch: [Channel::new(), Channel::new()],
            stereo: StereoState::default(),
            n_channels_api: 0,
            n_channels_internal: 0,
            prev_decode_only_middle: false,
            tmp: [[0; MAX_FRAME_LENGTH + 2]; 2],
            resampled: [0; 960],
        }
    }

    /// `silk_InitDecoder` (with the RFC 8251 stereo state reset).
    pub fn reset(&mut self) {
        self.ch = [Channel::new(), Channel::new()];
        self.stereo = StereoState::default();
        self.prev_decode_only_middle = false;
    }

    /// Decode one SILK frame (10 or 20 ms) of the packet. `ec` is `None` for a lost packet.
    /// Writes interleaved 48 kHz samples (`channels_api` channels) and returns the samples per
    /// channel written.
    pub fn decode(
        &mut self,
        ctl: &SilkControl,
        mut ec: Option<&mut RangeDecoder>,
        new_packet: bool,
        out: &mut [i16],
    ) -> usize {
        let lost = ec.is_none();
        if new_packet {
            for c in &mut self.ch[..ctl.channels_internal] {
                c.n_frames_decoded = 0;
            }
        }
        if ctl.channels_internal > self.n_channels_internal {
            self.ch[1] = Channel::new();
        }
        let stereo_to_mono = ctl.channels_internal == 1
            && self.n_channels_internal == 2
            && ctl.internal_rate == 1000 * self.ch[0].fs_khz;
        if self.ch[0].n_frames_decoded == 0 {
            for c in &mut self.ch[..ctl.channels_internal] {
                let (fpp, nsub) = match ctl.payload_ms {
                    40 => (2, 4),
                    60 => (3, 4),
                    20 => (1, 4),
                    _ => (1, 2),
                };
                c.n_frames_per_packet = fpp;
                c.nb_subfr = nsub;
                let fs_khz = (ctl.internal_rate >> 10) + 1;
                c.set_fs(fs_khz, 48000);
            }
        }
        if ctl.channels_api == 2
            && ctl.channels_internal == 2
            && (self.n_channels_api == 1 || self.n_channels_internal == 1)
        {
            self.stereo.pred_prev_q13 = [0; 2];
            self.stereo.s_side = [0; 2];
            self.ch[1].resampler = self.ch[0].resampler.clone();
        }
        self.n_channels_api = ctl.channels_api;
        self.n_channels_internal = ctl.channels_internal;

        let mut ms_pred_q13 = [0i32; 2];
        let mut decode_only_middle = false;
        if let Some(ec) = ec.as_deref_mut() {
            if self.ch[0].n_frames_decoded == 0 {
                // VAD and LBRR flags.
                for c in &mut self.ch[..ctl.channels_internal] {
                    for i in 0..c.n_frames_per_packet {
                        c.vad_flags[i] = ec.bit_logp(1);
                    }
                    c.lbrr_flag = ec.bit_logp(1);
                }
                for c in &mut self.ch[..ctl.channels_internal] {
                    c.lbrr_flags = [false; 3];
                    if c.lbrr_flag {
                        if c.n_frames_per_packet == 1 {
                            c.lbrr_flags[0] = true;
                        } else {
                            let tab: &[u8] = if c.n_frames_per_packet == 2 {
                                &LBRR_FLAGS_2_ICDF
                            } else {
                                &LBRR_FLAGS_3_ICDF
                            };
                            let sym = ec.icdf(tab, 8) + 1;
                            for i in 0..c.n_frames_per_packet {
                                c.lbrr_flags[i] = (sym >> i) & 1 != 0;
                            }
                        }
                    }
                }
                // Skip the LBRR (in-band FEC) frames.
                for i in 0..self.ch[0].n_frames_per_packet {
                    for n in 0..ctl.channels_internal {
                        if self.ch[n].lbrr_flags[i] {
                            if ctl.channels_internal == 2 && n == 0 {
                                let _ = stereo_decode_pred(ec);
                                if !self.ch[1].lbrr_flags[i] {
                                    let _ = ec.icdf(&STEREO_ONLY_CODE_MID_ICDF, 8);
                                }
                            }
                            let cond = if i > 0 && self.ch[n].lbrr_flags[i - 1] {
                                CODE_CONDITIONALLY
                            } else {
                                CODE_INDEPENDENTLY
                            };
                            let c = &mut self.ch[n];
                            c.decode_indices(ec, i, true, cond);
                            let mut pulses = [0i32; MAX_FRAME_LENGTH];
                            decode_pulses(
                                ec,
                                &mut pulses,
                                c.indices.signal_type,
                                c.indices.quant_offset_type,
                                c.frame_length,
                            );
                        }
                    }
                }
            }
            if ctl.channels_internal == 2 {
                ms_pred_q13 = stereo_decode_pred(ec);
                let fd = self.ch[0].n_frames_decoded.min(2);
                decode_only_middle = if !self.ch[1].vad_flags[fd] {
                    ec.icdf(&STEREO_ONLY_CODE_MID_ICDF, 8) != 0
                } else {
                    false
                };
            }
        } else if ctl.channels_internal == 2 {
            ms_pred_q13 = self.stereo.pred_prev_q13;
        }
        if ctl.channels_internal == 2 && !decode_only_middle && self.prev_decode_only_middle {
            let c = &mut self.ch[1];
            c.out_buf.fill(0);
            c.s_lpc_q14_buf.fill(0);
            c.lag_prev = 100;
            c.last_gain_index = 10;
            c.prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
            c.first_frame_after_reset = true;
        }
        let has_side = if !lost {
            !decode_only_middle
        } else {
            !self.prev_decode_only_middle
        };

        let mut n_out = 0usize;
        for n in 0..ctl.channels_internal {
            if n == 0 || has_side {
                let frame_index = self.ch[0].n_frames_decoded as i32 - n as i32;
                let cond = if frame_index <= 0 {
                    CODE_INDEPENDENTLY
                } else if n > 0 && self.prev_decode_only_middle {
                    CODE_INDEPENDENTLY_NO_LTP_SCALING
                } else {
                    CODE_CONDITIONALLY
                };
                let (tmp, ch) = (&mut self.tmp[n], &mut self.ch[n]);
                n_out = ch.decode_frame(ec.as_deref_mut(), &mut tmp[2..], false, cond);
            } else {
                let l = n_out.max(self.ch[0].frame_length);
                self.tmp[n][2..2 + l].fill(0);
            }
            self.ch[n].n_frames_decoded += 1;
        }
        if ctl.channels_api == 2 && ctl.channels_internal == 2 {
            let (a, b) = self.tmp.split_at_mut(1);
            stereo_ms_to_lr(
                &mut self.stereo,
                &mut a[0],
                &mut b[0],
                ms_pred_q13,
                self.ch[0].fs_khz,
                n_out,
            );
        } else {
            self.tmp[0][..2].copy_from_slice(&self.stereo.s_mid);
            self.stereo
                .s_mid
                .copy_from_slice(&self.tmp[0][n_out..n_out + 2]);
        }
        let fs_khz = self.ch[0].fs_khz.max(1) as usize;
        let n_samples_out = n_out * 48 / fs_khz;
        let api = ctl.channels_api;
        for n in 0..api.min(ctl.channels_internal) {
            if let Some(rs) = self.ch[n].resampler.as_mut() {
                rs.process(
                    &mut self.resampled[..n_samples_out],
                    &self.tmp[n][1..1 + n_out],
                );
            }
            for i in 0..n_samples_out {
                out[n + api * i] = self.resampled[i];
            }
        }
        if api == 2 && ctl.channels_internal == 1 {
            if stereo_to_mono && self.ch[1].resampler.is_some() {
                if let Some(rs) = self.ch[1].resampler.as_mut() {
                    rs.process(
                        &mut self.resampled[..n_samples_out],
                        &self.tmp[0][1..1 + n_out],
                    );
                }
                for i in 0..n_samples_out {
                    out[1 + 2 * i] = self.resampled[i];
                }
            } else {
                for i in 0..n_samples_out {
                    out[1 + 2 * i] = out[2 * i];
                }
            }
        }
        if lost {
            for c in &mut self.ch[..self.n_channels_internal] {
                c.last_gain_index = 10;
            }
        } else {
            self.prev_decode_only_middle = decode_only_middle;
        }
        n_samples_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_point_helpers() {
        assert_eq!(rshift_round(5, 1), 3);
        assert_eq!(rshift_round(-5, 1), -2);
        assert_eq!(rshift_round(1000, 4), 63);
        assert_eq!(smulwb(65536 * 3, 7), 21);
        assert_eq!(log2lin(0), 1);
        assert_eq!(log2lin(7 << 7), 128);
        let inv = inverse32_varq(1 << 16, 47);
        assert!(inv >= i32::MAX - 2, "{inv}");
        assert!((div32_varq(1 << 20, 1 << 10, 16) - (1 << 26)).abs() <= 2);
        assert_eq!(sqrt_approx(0), 0);
        let s = sqrt_approx(1 << 20);
        assert!((s - 1024).abs() < 30, "{s}");
    }

    #[test]
    fn nlsf2a_is_stable() {
        // Evenly spaced NLSFs give a flat (near-zero) predictor.
        for &d in &[10usize, 16] {
            let nlsf: Vec<i16> = (0..d)
                .map(|i| ((i as i32 + 1) * 32768 / (d as i32 + 1)) as i16)
                .collect();
            let mut a = [0i16; 16];
            nlsf2a(&mut a[..d], &nlsf);
            assert!(lpc_inverse_pred_gain(&a[..d]) > 0);
            for &v in &a[..d] {
                assert!(v.abs() < 1000, "{a:?}");
            }
        }
    }

    #[test]
    fn resampler_ratio() {
        for fs in [8000, 12000, 16000] {
            let mut r = Resampler::new(fs, 48000);
            let n_in = (fs / 50) as usize;
            let input: Vec<i16> = (0..n_in)
                .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
                .collect();
            let mut out = vec![0i16; 960];
            r.process(&mut out, &input);
            assert!(out.iter().any(|&v| v != 0));
        }
    }
}

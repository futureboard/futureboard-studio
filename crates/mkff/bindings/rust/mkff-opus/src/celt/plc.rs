//! CELT packet loss concealment helpers: pitch search on the decoded history and LPC
//! extrapolation (`pitch.c`, `celt_lpc.c` of the reference implementation, float build; see
//! `NOTICE`).

// Ported fixed-/floating-point code keeps the reference implementation's expression and loop
// structure, which these lints would obscure.
#![allow(
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::needless_late_init
)]

pub const LPC_ORDER: usize = 24;

/// `pitch_downsample`: 2x decimated, whitened mix of the channels' history.
pub fn pitch_downsample(x: &[&[f32]], x_lp: &mut [f32], len: usize) {
    let half = len >> 1;
    x_lp[..half].fill(0.0);
    for ch in x {
        for i in 1..half {
            x_lp[i] += 0.5 * (0.5 * (ch[2 * i - 1] + ch[2 * i + 1]) + ch[2 * i]);
        }
        x_lp[0] += 0.5 * (0.5 * ch[1] + ch[0]);
    }
    let mut ac = [0f32; 5];
    autocorr(&x_lp[..half], &mut ac, None, 4);
    ac[0] *= 1.0001;
    for (i, a) in ac.iter_mut().enumerate().skip(1) {
        let f = 0.008 * i as f32;
        *a -= *a * f * f;
    }
    let mut lpc = [0f32; 4];
    celt_lpc(&mut lpc, &ac, 4);
    let mut tmp = 1.0f32;
    for l in lpc.iter_mut() {
        tmp *= 0.9;
        *l *= tmp;
    }
    let mut mem = [0f32; LPC_ORDER];
    fir_in_place(&mut x_lp[..half], &lpc, &mut mem);
    let mut mem = [0f32; LPC_ORDER];
    fir_in_place(&mut x_lp[..half], &[0.8], &mut mem);
}

fn find_best_pitch(xcorr: &[f32], y: &[f32], len: usize, max_pitch: usize) -> [usize; 2] {
    let mut syy = 1f32;
    let mut best_num = [-1f32; 2];
    let mut best_den = [0f32; 2];
    let mut best_pitch = [0usize, 1];
    for v in &y[..len] {
        syy += v * v;
    }
    for i in 0..max_pitch {
        if xcorr[i] > 0.0 {
            let x16 = xcorr[i] * 1e-12;
            let num = x16 * x16;
            if num * best_den[1] > best_num[1] * syy {
                if num * best_den[0] > best_num[0] * syy {
                    best_num[1] = best_num[0];
                    best_den[1] = best_den[0];
                    best_pitch[1] = best_pitch[0];
                    best_num[0] = num;
                    best_den[0] = syy;
                    best_pitch[0] = i;
                } else {
                    best_num[1] = num;
                    best_den[1] = syy;
                    best_pitch[1] = i;
                }
            }
        }
        syy += y[i + len] * y[i + len] - y[i] * y[i];
        syy = syy.max(1.0);
    }
    best_pitch
}

/// `pitch_search`: the lag (in 2x-decimated samples, ×2) maximising the normalised
/// correlation between `x_lp` and `y`.
pub fn pitch_search(
    x_lp: &[f32],
    y: &[f32],
    len: usize,
    max_pitch: usize,
    xcorr: &mut [f32],
) -> usize {
    let lag = len + max_pitch;
    let mut x4 = [0f32; 512];
    let mut y4 = [0f32; 512];
    let (n4, l4) = ((len >> 2).min(512), (lag >> 2).min(512));
    for j in 0..n4 {
        x4[j] = x_lp[2 * j];
    }
    for j in 0..l4 {
        y4[j] = y[2 * j];
    }
    let x4 = &x4[..n4];
    let y4 = &y4[..l4];
    for i in 0..max_pitch >> 2 {
        let s: f32 = x4.iter().zip(&y4[i..]).map(|(a, b)| a * b).sum();
        xcorr[i] = s.max(-1.0);
    }
    let best = find_best_pitch(xcorr, y4, len >> 2, max_pitch >> 2);
    for i in 0..max_pitch >> 1 {
        xcorr[i] = 0.0;
        if (i as isize - 2 * best[0] as isize).abs() > 2
            && (i as isize - 2 * best[1] as isize).abs() > 2
        {
            continue;
        }
        let s: f32 = x_lp[..len >> 1]
            .iter()
            .zip(&y[i..])
            .map(|(a, b)| a * b)
            .sum();
        xcorr[i] = s.max(-1.0);
    }
    let best = find_best_pitch(xcorr, y, len >> 1, max_pitch >> 1);
    let offset: isize = if best[0] > 0 && best[0] < (max_pitch >> 1) - 1 {
        let a = xcorr[best[0] - 1];
        let b = xcorr[best[0]];
        let c = xcorr[best[0] + 1];
        if c - a > 0.7 * (b - a) {
            1
        } else if a - c > 0.7 * (b - c) {
            -1
        } else {
            0
        }
    } else {
        0
    };
    (2 * best[0] as isize - offset).max(0) as usize
}

/// `_celt_autocorr` (float) with an optional symmetric window of `window.len()` samples.
pub fn autocorr(x: &[f32], ac: &mut [f32], window: Option<&[f32]>, lag: usize) {
    let n = x.len();
    let w = |i: usize| -> f32 {
        match window {
            Some(win) if i < win.len() => win[i],
            Some(win) if i >= n - win.len() => win[n - 1 - i],
            _ => 1.0,
        }
    };
    for (l, a) in ac.iter_mut().enumerate().take(lag + 1) {
        let mut d = 0f32;
        for i in l..n {
            d += x[i] * w(i) * x[i - l] * w(i - l);
        }
        *a = d;
    }
    ac[0] += 10.0;
}

/// `_celt_lpc` (float Levinson-Durbin).
pub fn celt_lpc(lpc: &mut [f32], ac: &[f32], p: usize) {
    let mut error = ac[0];
    lpc[..p].fill(0.0);
    if ac[0] == 0.0 {
        return;
    }
    for i in 0..p {
        let mut rr = 0f32;
        for j in 0..i {
            rr += lpc[j] * ac[i - j];
        }
        rr += ac[i + 1];
        let r = -rr / error;
        lpc[i] = r;
        for j in 0..(i + 1) >> 1 {
            let t1 = lpc[j];
            let t2 = lpc[i - 1 - j];
            lpc[j] = t1 + r * t2;
            lpc[i - 1 - j] = t2 + r * t1;
        }
        error -= r * r * error;
        if error < 0.001 * ac[0] {
            break;
        }
    }
}

/// `celt_fir` in place: `y[i] = x[i] + Σ num[j] x[i-1-j]` (with `mem` the past input).
pub fn fir_in_place(x: &mut [f32], num: &[f32], mem: &mut [f32; LPC_ORDER]) {
    let ord = num.len();
    for v in x.iter_mut() {
        let xi = *v;
        let mut sum = xi;
        for j in 0..ord {
            sum += num[j] * mem[j];
        }
        mem.copy_within(0..ord - 1, 1);
        mem[0] = xi;
        *v = sum;
    }
}

/// `celt_iir` in place: `y[i] = x[i] - Σ den[j] y[i-1-j]`.
pub fn iir_in_place(x: &mut [f32], den: &[f32], mem: &mut [f32; LPC_ORDER]) {
    let ord = den.len();
    for v in x.iter_mut() {
        let mut sum = *v;
        for j in 0..ord {
            sum -= den[j] * mem[j];
        }
        mem.copy_within(0..ord - 1, 1);
        mem[0] = sum;
        *v = sum;
    }
}

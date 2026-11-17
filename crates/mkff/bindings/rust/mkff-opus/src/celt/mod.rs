//! The CELT layer (RFC 6716 §4.3), float implementation following the reference decoder
//! (`celt.c`, `quant_bands.c`; see `NOTICE`).

// Ported fixed-/floating-point code keeps the reference implementation's expression and loop
// structure, which these lints would obscure.
#![allow(
    clippy::precedence,
    clippy::needless_range_loop,
    clippy::needless_late_init
)]

mod bands;
mod plc;
mod rate;
mod tables;

use mkff_audio_core::fft::{Complex, Fft};
use mkff_audio_core::{Error, Result};

use crate::range::{RangeDecoder, BITRES};
use bands::{AllBandsParams, BandScratch};
use tables::{EBANDS, E_PROB_MODEL, WINDOW};

/// Number of CELT bands (48 kHz, 20 ms mode).
pub const NB_EBANDS: usize = 21;
/// Overlap of consecutive MDCT windows.
pub const OVERLAP: usize = 120;
const DECODE_BUFFER_SIZE: usize = 2048;
const COMBFILTER_MINPERIOD: i32 = 15;
const PREEMPH: f32 = 0.850_006_1;
/// Scale of the CELT signal domain relative to [-1, 1] float.
const SIG_SCALE: f32 = 32768.0;

/// Mean band energies in log2 units (the reference fixed-point table divided by 16).
const E_MEANS: [f32; 25] = [
    103.0 / 16.0,
    100.0 / 16.0,
    92.0 / 16.0,
    85.0 / 16.0,
    81.0 / 16.0,
    77.0 / 16.0,
    72.0 / 16.0,
    70.0 / 16.0,
    78.0 / 16.0,
    75.0 / 16.0,
    73.0 / 16.0,
    71.0 / 16.0,
    78.0 / 16.0,
    74.0 / 16.0,
    69.0 / 16.0,
    72.0 / 16.0,
    70.0 / 16.0,
    74.0 / 16.0,
    76.0 / 16.0,
    71.0 / 16.0,
    60.0 / 16.0,
    60.0 / 16.0,
    60.0 / 16.0,
    60.0 / 16.0,
    60.0 / 16.0,
];
const PRED_COEF: [f32; 4] = [
    29440.0 / 32768.0,
    26112.0 / 32768.0,
    21248.0 / 32768.0,
    16384.0 / 32768.0,
];
const BETA_COEF: [f32; 4] = [
    30147.0 / 32768.0,
    22282.0 / 32768.0,
    12124.0 / 32768.0,
    6554.0 / 32768.0,
];
const BETA_INTRA: f32 = 4915.0 / 32768.0;
const SMALL_ENERGY_ICDF: [u8; 3] = [2, 1, 0];
const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];
const SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];
const TAPSET_ICDF: [u8; 3] = [2, 1, 0];
const TF_SELECT_TABLE: [[i8; 8]; 4] = [
    [0, -1, 0, -1, 0, -1, 0, -1],
    [0, -1, 0, -2, 1, 0, 1, -1],
    [0, -2, 0, -3, 2, 0, 1, -1],
    [0, -2, 0, -3, 3, 0, 1, -1],
];
const COMB_GAINS: [[f32; 3]; 3] = [
    [0.306_640_63, 0.217_041_02, 0.129_638_67],
    [0.463_867_2, 0.268_066_4, 0.0],
    [0.799_804_7, 0.100_097_656, 0.0],
];
const MAX_FINE_BITS: i32 = 8;

/// The 120-sample CELT overlap window.
pub fn window() -> &'static [f32; OVERLAP] {
    &WINDOW
}

#[inline]
fn eb(i: usize) -> usize {
    EBANDS[i] as usize
}

/// The CELT inverse MDCT (`clt_mdct_backward`) of `n2` coefficients with the 120-sample
/// low-overlap window, computed exactly through an `n2 / 2`-point complex FFT.
struct Imdct {
    n2: usize,
    fft: Fft,
    tw: Vec<Complex>,
    buf: Vec<Complex>,
    f2: Vec<f32>,
}

impl Imdct {
    fn new(n2: usize) -> Self {
        let n = 2 * n2;
        let n4 = n2 / 2;
        let tw = (0..n4)
            .map(|i| Complex::expi(2.0 * std::f64::consts::PI * (i as f64 + 0.125) / n as f64))
            .collect();
        Imdct {
            n2,
            fft: Fft::new(n4),
            tw,
            buf: vec![Complex::ZERO; n4],
            f2: vec![0.0; n2],
        }
    }

    /// `input[k * stride]` for `k < n2` → `out[..n2 + OVERLAP]`; the first `OVERLAP` samples are
    /// added to what `out` holds, the rest are overwritten.
    fn backward(&mut self, input: &[f32], stride: usize, out: &mut [f32]) {
        let n2 = self.n2;
        let n4 = n2 / 2;
        for i in 0..n4 {
            let x1 = input[2 * i * stride];
            let x2 = input[(n2 - 1 - 2 * i) * stride];
            self.buf[i] = Complex::new(-x2, -x1) * self.tw[i];
        }
        self.fft.inverse(&mut self.buf);
        for i in 0..n4 {
            self.buf[i] = self.buf[i] * self.tw[i];
        }
        for i in 0..n4 {
            self.f2[2 * i] = -self.buf[i].re;
            self.f2[2 * i + 1] = self.buf[n4 - 1 - i].im;
        }
        let f2 = &self.f2;
        let ov = OVERLAP;
        let h = ov / 2;
        let w = &WINDOW;
        for p in 0..h {
            out[p] += -w[p] * f2[h - 1 - p];
        }
        for p in h..ov {
            out[p] += w[p] * f2[p - h];
        }
        out[ov..n2].copy_from_slice(&f2[ov - h..n2 - h]);
        for p in n2..n2 + h {
            out[p] = w[ov - 1 - (p - n2)] * f2[p - h];
        }
        for p in n2 + h..n2 + ov {
            out[p] = w[n2 + ov - 1 - p] * f2[2 * n2 + h - 1 - p];
        }
    }
}

/// `comb_filter` applied in place on `buf[start..start + n]` (the CELT pitch post-filter).
#[allow(clippy::too_many_arguments)]
fn comb_filter(
    buf: &mut [f32],
    start: usize,
    t0: i32,
    t1: i32,
    n: usize,
    g0: f32,
    g1: f32,
    tapset0: usize,
    tapset1: usize,
) {
    if g0 == 0.0 && g1 == 0.0 {
        return;
    }
    let t0 = t0.max(COMBFILTER_MINPERIOD) as usize;
    let t1 = t1.max(COMBFILTER_MINPERIOD) as usize;
    let g00 = g0 * COMB_GAINS[tapset0][0];
    let g01 = g0 * COMB_GAINS[tapset0][1];
    let g02 = g0 * COMB_GAINS[tapset0][2];
    let g10 = g1 * COMB_GAINS[tapset1][0];
    let g11 = g1 * COMB_GAINS[tapset1][1];
    let g12 = g1 * COMB_GAINS[tapset1][2];
    let overlap = if g0 == g1 && t0 == t1 && tapset0 == tapset1 {
        0
    } else {
        OVERLAP.min(n)
    };
    for i in 0..overlap {
        let k = start + i;
        let f = WINDOW[i] * WINDOW[i];
        let nf = 1.0 - f;
        buf[k] = buf[k]
            + nf * g00 * buf[k - t0]
            + nf * g01 * (buf[k - t0 + 1] + buf[k - t0 - 1])
            + nf * g02 * (buf[k - t0 + 2] + buf[k - t0 - 2])
            + f * g10 * buf[k - t1]
            + f * g11 * (buf[k - t1 + 1] + buf[k - t1 - 1])
            + f * g12 * (buf[k - t1 + 2] + buf[k - t1 - 2]);
    }
    if g1 == 0.0 {
        return;
    }
    for i in overlap..n {
        let k = start + i;
        buf[k] = buf[k]
            + g10 * buf[k - t1]
            + g11 * (buf[k - t1 + 1] + buf[k - t1 - 1])
            + g12 * (buf[k - t1 + 2] + buf[k - t1 - 2]);
    }
}

/// The Laplace-distributed coarse energy symbol (`ec_laplace_decode`).
fn laplace_decode(ec: &mut RangeDecoder, fs: u32, decay: u32) -> i32 {
    const LAPLACE_MINP: u32 = 1;
    const LAPLACE_NMIN: u32 = 16;
    let get_freq1 = |fs0: u32| -> u32 {
        let ft = 32768 - LAPLACE_MINP * (2 * LAPLACE_NMIN) - fs0;
        (ft * (16384 - decay)) >> 15
    };
    let mut val = 0i32;
    let fm = ec.decode_bin(15);
    let mut fl = 0u32;
    let mut fs = fs;
    if fm >= fs {
        val += 1;
        fl = fs;
        fs = get_freq1(fs) + LAPLACE_MINP;
        while fs > LAPLACE_MINP && fm >= fl + 2 * fs {
            fs *= 2;
            fl += fs;
            fs = ((fs - 2 * LAPLACE_MINP) * decay) >> 15;
            fs += LAPLACE_MINP;
            val += 1;
        }
        if fs <= LAPLACE_MINP {
            let di = (fm - fl) >> 1;
            val += di as i32;
            fl += 2 * di * LAPLACE_MINP;
        }
        if fm < fl + fs {
            val = -val;
        } else {
            fl += fs;
        }
    }
    ec.update(fl, (fl + fs).min(32768), 32768);
    val
}

/// The CELT decoder state for one (mono or stereo) Opus stream.
pub struct CeltDecoder {
    channels: usize,
    stream_channels: usize,
    start: usize,
    end: usize,
    rng: u32,
    loss_count: u32,
    postfilter_period: i32,
    postfilter_period_old: i32,
    postfilter_gain: f32,
    postfilter_gain_old: f32,
    postfilter_tapset: usize,
    postfilter_tapset_old: usize,
    preemph_mem: [f32; 2],
    decode_mem: [Vec<f32>; 2],
    old_band_e: [f32; 2 * NB_EBANDS],
    old_log_e: [f32; 2 * NB_EBANDS],
    old_log_e2: [f32; 2 * NB_EBANDS],
    background_log_e: [f32; 2 * NB_EBANDS],
    // Scratch.
    x: Vec<f32>,
    freq: Vec<f32>,
    synth: Vec<f32>,
    collapse: Vec<u8>,
    band_scratch: BandScratch,
    imdct: Vec<Imdct>,
    disable_inv: bool,
    last_pitch_index: usize,
    lpc: [[f32; plc::LPC_ORDER]; 2],
    plc_scratch: Vec<f32>,
}

impl CeltDecoder {
    /// A decoder producing `channels` (1 or 2) output channels at 48 kHz.
    pub fn new(channels: usize) -> Self {
        let mut d = CeltDecoder {
            channels,
            stream_channels: channels,
            start: 0,
            end: NB_EBANDS,
            rng: 0,
            loss_count: 0,
            postfilter_period: 0,
            postfilter_period_old: 0,
            postfilter_gain: 0.0,
            postfilter_gain_old: 0.0,
            postfilter_tapset: 0,
            postfilter_tapset_old: 0,
            preemph_mem: [0.0; 2],
            decode_mem: [
                vec![0.0; DECODE_BUFFER_SIZE + OVERLAP],
                vec![0.0; DECODE_BUFFER_SIZE + OVERLAP],
            ],
            old_band_e: [0.0; 2 * NB_EBANDS],
            old_log_e: [-28.0; 2 * NB_EBANDS],
            old_log_e2: [-28.0; 2 * NB_EBANDS],
            background_log_e: [0.0; 2 * NB_EBANDS],
            x: vec![0.0; 2 * 960],
            freq: vec![0.0; 2 * 960],
            synth: vec![0.0; 960 + OVERLAP],
            collapse: vec![0; 2 * NB_EBANDS],
            band_scratch: BandScratch::new(),
            imdct: vec![
                Imdct::new(120),
                Imdct::new(240),
                Imdct::new(480),
                Imdct::new(960),
            ],
            disable_inv: channels == 1,
            last_pitch_index: 0,
            lpc: [[0.0; plc::LPC_ORDER]; 2],
            plc_scratch: vec![0.0; 1024 + 1264 + 1024 + 512],
        };
        d.reset();
        d
    }

    /// `OPUS_RESET_STATE`.
    pub fn reset(&mut self) {
        self.rng = 0;
        self.loss_count = 0;
        self.postfilter_period = 0;
        self.postfilter_period_old = 0;
        self.postfilter_gain = 0.0;
        self.postfilter_gain_old = 0.0;
        self.postfilter_tapset = 0;
        self.postfilter_tapset_old = 0;
        self.preemph_mem = [0.0; 2];
        for m in &mut self.decode_mem {
            m.fill(0.0);
        }
        self.old_band_e = [0.0; 2 * NB_EBANDS];
        self.old_log_e = [-28.0; 2 * NB_EBANDS];
        self.old_log_e2 = [-28.0; 2 * NB_EBANDS];
        self.background_log_e = [0.0; 2 * NB_EBANDS];
    }

    /// First band decoded (17 in hybrid mode).
    pub fn set_start_band(&mut self, start: usize) {
        self.start = start.min(NB_EBANDS - 1);
    }

    /// One past the last band decoded (from the bandwidth).
    pub fn set_end_band(&mut self, end: usize) {
        self.end = end.clamp(1, NB_EBANDS);
    }

    /// Channels coded in the stream (may differ from the output channel count).
    pub fn set_stream_channels(&mut self, c: usize) {
        self.stream_channels = c.clamp(1, 2);
    }

    /// The final range coder state of the last decoded frame.
    pub fn final_range(&self) -> u32 {
        self.rng
    }

    fn lm_for(frame_size: usize) -> Result<usize> {
        match frame_size {
            120 => Ok(0),
            240 => Ok(1),
            480 => Ok(2),
            960 => Ok(3),
            _ => Err(Error::invalid(format!("celt: bad frame size {frame_size}"))),
        }
    }

    fn unquant_coarse_energy(
        &mut self,
        intra: bool,
        ec: &mut RangeDecoder,
        c: usize,
        lm: usize,
        budget: i32,
    ) {
        let prob = &E_PROB_MODEL[(lm * 2 + usize::from(intra)) * 42..];
        let (coef, beta) = if intra {
            (0.0, BETA_INTRA)
        } else {
            (PRED_COEF[lm], BETA_COEF[lm])
        };
        let mut prev = [0f32; 2];
        for i in self.start..self.end {
            for ch in 0..c {
                let tell = ec.tell();
                let qi = if budget - tell >= 15 {
                    let pi = 2 * i.min(20);
                    laplace_decode(ec, u32::from(prob[pi]) << 7, u32::from(prob[pi + 1]) << 6)
                } else if budget - tell >= 2 {
                    let qi = ec.icdf(&SMALL_ENERGY_ICDF, 2) as i32;
                    (qi >> 1) ^ -(qi & 1)
                } else if budget - tell >= 1 {
                    -i32::from(ec.bit_logp(1))
                } else {
                    -1
                };
                let q = qi as f32;
                let idx = i + ch * NB_EBANDS;
                self.old_band_e[idx] = self.old_band_e[idx].max(-9.0);
                let tmp = coef * self.old_band_e[idx] + prev[ch] + q;
                self.old_band_e[idx] = tmp;
                prev[ch] = prev[ch] + q - beta * q;
            }
        }
    }

    fn tf_decode(
        &self,
        transient: bool,
        tf_res: &mut [i32; NB_EBANDS],
        lm: usize,
        ec: &mut RangeDecoder,
    ) {
        let mut budget = ec.storage() as u32 * 8;
        let mut tell = ec.tell() as u32;
        let mut logp = if transient { 2 } else { 4 };
        let tf_select_rsv = lm > 0 && tell + logp < budget;
        budget -= u32::from(tf_select_rsv);
        let mut tf_changed = 0;
        let mut curr = 0;
        for r in tf_res.iter_mut().take(self.end).skip(self.start) {
            if tell + logp <= budget {
                curr ^= i32::from(ec.bit_logp(logp));
                tell = ec.tell() as u32;
                tf_changed |= curr;
            }
            *r = curr;
            logp = if transient { 4 } else { 5 };
        }
        let ti = 4 * usize::from(transient);
        let mut tf_select = 0;
        if tf_select_rsv
            && TF_SELECT_TABLE[lm][ti + tf_changed as usize]
                != TF_SELECT_TABLE[lm][ti + 2 + tf_changed as usize]
        {
            tf_select = usize::from(ec.bit_logp(1));
        }
        for r in tf_res.iter_mut().take(self.end).skip(self.start) {
            *r = i32::from(TF_SELECT_TABLE[lm][ti + 2 * tf_select + *r as usize]);
        }
    }

    /// Decode one CELT frame of `frame_size` samples (120–960) from `ec`, whose buffer is `len`
    /// bytes, into `pcm` (interleaved, `channels` output channels, float in [-1, 1]).
    pub fn decode(
        &mut self,
        ec: &mut RangeDecoder,
        len: usize,
        pcm: &mut [f32],
        frame_size: usize,
    ) -> Result<()> {
        let lm = Self::lm_for(frame_size)?;
        let cc = self.channels;
        let c = self.stream_channels;
        let m = 1usize << lm;
        let n = m * 120;
        let start = self.start;
        let end = self.end;
        let eff_end = end.min(NB_EBANDS);
        if pcm.len() < n * cc {
            return Err(Error::InvalidArgument(
                "celt: output buffer too small".into(),
            ));
        }
        if len <= 1 {
            self.decode_lost(pcm, frame_size);
            return Ok(());
        }
        if c == 1 {
            for i in 0..NB_EBANDS {
                self.old_band_e[i] = self.old_band_e[i].max(self.old_band_e[NB_EBANDS + i]);
            }
        }
        let total_bits_raw = (len * 8) as i32;
        let mut tell = ec.tell();
        let silence = if tell >= total_bits_raw {
            true
        } else if tell == 1 {
            ec.bit_logp(15)
        } else {
            false
        };
        if silence {
            ec.consume_all();
            tell = total_bits_raw;
        }
        let mut postfilter_gain = 0.0f32;
        let mut postfilter_pitch = 0i32;
        let mut postfilter_tapset = 0usize;
        if start == 0 && tell + 16 <= total_bits_raw {
            if ec.bit_logp(1) {
                let octave = ec.uint(6);
                postfilter_pitch = ((16u32 << octave) + ec.bits(4 + octave)) as i32 - 1;
                let qg = ec.bits(3);
                if ec.tell() + 2 <= total_bits_raw {
                    postfilter_tapset = ec.icdf(&TAPSET_ICDF, 2);
                }
                postfilter_gain = 0.09375 * (qg + 1) as f32;
            }
            tell = ec.tell();
        }
        let mut transient = false;
        if lm > 0 && tell + 3 <= total_bits_raw {
            transient = ec.bit_logp(3);
            tell = ec.tell();
        }
        let short_blocks = transient;
        let intra = if tell + 3 <= total_bits_raw {
            ec.bit_logp(3)
        } else {
            false
        };

        let budget = (ec.storage() * 8) as i32;
        self.unquant_coarse_energy(intra, ec, c, lm, budget);

        let mut tf_res = [0i32; NB_EBANDS];
        self.tf_decode(transient, &mut tf_res, lm, ec);

        tell = ec.tell();
        let mut spread = 2;
        if tell + 4 <= total_bits_raw {
            spread = ec.icdf(&SPREAD_ICDF, 5) as i32;
        }
        let mut cap = [0i32; NB_EBANDS];
        rate::init_caps(&mut cap, lm as i32, c as i32);
        let mut offsets = [0i32; NB_EBANDS];
        let mut dynalloc_logp = 6;
        let mut total_bits = total_bits_raw << BITRES;
        let mut tell_frac = ec.tell_frac() as i32;
        for i in start..end {
            let width = (c * (eb(i + 1) - eb(i)) << lm) as i32;
            let quanta = (width << BITRES).min((6 << BITRES).max(width));
            let mut loop_logp = dynalloc_logp;
            let mut boost = 0;
            while tell_frac + (loop_logp << BITRES) < total_bits && boost < cap[i] {
                let flag = ec.bit_logp(loop_logp as u32);
                tell_frac = ec.tell_frac() as i32;
                if !flag {
                    break;
                }
                boost += quanta;
                total_bits -= quanta;
                loop_logp = 1;
            }
            offsets[i] = boost;
            if boost > 0 {
                dynalloc_logp = 2.max(dynalloc_logp - 1);
            }
        }
        let alloc_trim = if tell_frac + (6 << BITRES) <= total_bits {
            ec.icdf(&TRIM_ICDF, 7) as i32
        } else {
            5
        };
        let mut bits = ((len as i32 * 8) << BITRES) - ec.tell_frac() as i32 - 1;
        let anti_collapse_rsv = if transient && lm >= 2 && bits >= ((lm as i32 + 2) << BITRES) {
            1 << BITRES
        } else {
            0
        };
        bits -= anti_collapse_rsv;
        let alloc = rate::compute_allocation(
            start, end, &offsets, &cap, alloc_trim, bits, c as i32, lm as i32, ec,
        );

        // Fine energy.
        for i in start..end {
            let fq = alloc.fine_quant[i];
            if fq <= 0 {
                continue;
            }
            for ch in 0..c {
                let q2 = ec.bits(fq as u32);
                let offset = (q2 as f32 + 0.5) * (1 << (14 - fq)) as f32 * (1.0 / 16384.0) - 0.5;
                self.old_band_e[i + ch * NB_EBANDS] += offset;
            }
        }

        // Shapes.
        let x = &mut self.x[..c * n];
        for ch in 0..c {
            x[ch * n..ch * n + m * eb(start)].fill(0.0);
            x[ch * n + m * eb(eff_end)..(ch + 1) * n].fill(0.0);
        }
        let params = AllBandsParams {
            start,
            end,
            pulses: &alloc.pulses,
            short_blocks,
            spread,
            dual_stereo: alloc.dual_stereo,
            intensity: alloc.intensity,
            tf_res: &tf_res,
            total_bits: (len as i32 * (8 << BITRES)) - anti_collapse_rsv,
            balance: alloc.balance,
            lm: lm as i32,
            coded_bands: alloc.coded_bands,
            disable_inv: self.disable_inv,
        };
        {
            let (xa, xb) = x.split_at_mut(n);
            let y = if c == 2 { Some(&mut xb[..n]) } else { None };
            self.rng = bands::quant_all_bands(
                &params,
                xa,
                y,
                &mut self.collapse,
                ec,
                self.rng,
                &mut self.band_scratch,
            );
        }
        let anti_collapse_on = anti_collapse_rsv > 0 && ec.bits(1) != 0;

        // Final fine energy bits.
        let mut bits_left = len as i32 * 8 - ec.tell();
        for prio in 0..2 {
            let mut i = start;
            while i < end && bits_left >= c as i32 {
                if alloc.fine_quant[i] >= MAX_FINE_BITS || alloc.fine_priority[i] != prio {
                    i += 1;
                    continue;
                }
                for ch in 0..c {
                    let q2 = ec.bits(1);
                    let offset = (q2 as f32 - 0.5)
                        * (1 << (14 - alloc.fine_quant[i] - 1)) as f32
                        * (1.0 / 16384.0);
                    self.old_band_e[i + ch * NB_EBANDS] += offset;
                    bits_left -= 1;
                }
                i += 1;
            }
        }

        if anti_collapse_on {
            bands::anti_collapse(
                x,
                &self.collapse,
                lm as i32,
                c,
                n,
                start,
                end,
                &self.old_band_e,
                &self.old_log_e,
                &self.old_log_e2,
                &alloc.pulses,
                self.rng,
            );
        }

        if silence {
            for v in self.old_band_e.iter_mut().take(c * NB_EBANDS) {
                *v = -28.0;
            }
        }

        // Denormalise.
        let freq = &mut self.freq[..cc.max(c) * n];
        for ch in 0..c {
            let f = &mut freq[ch * n..(ch + 1) * n];
            f.fill(0.0);
            if !silence {
                for i in start..eff_end {
                    let lg = (self.old_band_e[i + ch * NB_EBANDS] + E_MEANS[i]).min(32.0);
                    let g = lg.exp2();
                    for j in m * eb(i)..m * eb(i + 1) {
                        f[j] = x[ch * n + j] * g;
                    }
                }
            }
        }
        if cc == 2 && c == 1 {
            freq.copy_within(0..n, n);
        }
        if cc == 1 && c == 2 {
            for i in 0..n {
                freq[i] = 0.5 * (freq[i] + freq[n + i]);
            }
        }

        // Inverse MDCT into the decode buffers.
        for ch in 0..cc {
            let mem = &mut self.decode_mem[ch];
            mem.copy_within(n..DECODE_BUFFER_SIZE, 0);
            let synth = &mut self.synth[..n + OVERLAP];
            synth[..OVERLAP].fill(0.0);
            let fr = &freq[ch * n..(ch + 1) * n];
            if short_blocks {
                let imdct = &mut self.imdct[0];
                for b in 0..m {
                    imdct.backward(&fr[b..], m, &mut synth[120 * b..120 * b + 120 + OVERLAP]);
                }
            } else {
                self.imdct[lm].backward(fr, 1, synth);
            }
            let base = DECODE_BUFFER_SIZE - n;
            for j in 0..OVERLAP {
                mem[base + j] = synth[j] + mem[DECODE_BUFFER_SIZE + j];
            }
            mem[base + OVERLAP..DECODE_BUFFER_SIZE].copy_from_slice(&synth[OVERLAP..n]);
            mem[DECODE_BUFFER_SIZE..DECODE_BUFFER_SIZE + OVERLAP]
                .copy_from_slice(&synth[n..n + OVERLAP]);
        }

        // Pitch post-filter.
        self.postfilter_period = self.postfilter_period.max(COMBFILTER_MINPERIOD);
        self.postfilter_period_old = self.postfilter_period_old.max(COMBFILTER_MINPERIOD);
        for ch in 0..cc {
            let mem = &mut self.decode_mem[ch];
            let base = DECODE_BUFFER_SIZE - n;
            comb_filter(
                mem,
                base,
                self.postfilter_period_old,
                self.postfilter_period,
                120,
                self.postfilter_gain_old,
                self.postfilter_gain,
                self.postfilter_tapset_old,
                self.postfilter_tapset,
            );
            if lm != 0 {
                comb_filter(
                    mem,
                    base + 120,
                    self.postfilter_period,
                    postfilter_pitch,
                    n - 120,
                    self.postfilter_gain,
                    postfilter_gain,
                    self.postfilter_tapset,
                    postfilter_tapset,
                );
            }
        }
        self.postfilter_period_old = self.postfilter_period;
        self.postfilter_gain_old = self.postfilter_gain;
        self.postfilter_tapset_old = self.postfilter_tapset;
        self.postfilter_period = postfilter_pitch;
        self.postfilter_gain = postfilter_gain;
        self.postfilter_tapset = postfilter_tapset;
        if lm != 0 {
            self.postfilter_period_old = self.postfilter_period;
            self.postfilter_gain_old = self.postfilter_gain;
            self.postfilter_tapset_old = self.postfilter_tapset;
        }

        if c == 1 {
            let (a, b) = self.old_band_e.split_at_mut(NB_EBANDS);
            b.copy_from_slice(a);
        }
        if !transient {
            self.old_log_e2 = self.old_log_e;
            self.old_log_e = self.old_band_e;
            for i in 0..2 * NB_EBANDS {
                self.background_log_e[i] =
                    (self.background_log_e[i] + m as f32 * 0.001).min(self.old_band_e[i]);
            }
        } else {
            for i in 0..2 * NB_EBANDS {
                self.old_log_e[i] = self.old_log_e[i].min(self.old_band_e[i]);
            }
        }
        for ch in 0..2 {
            for i in (0..start).chain(end..NB_EBANDS) {
                let k = ch * NB_EBANDS + i;
                self.old_band_e[k] = 0.0;
                self.old_log_e[k] = -28.0;
                self.old_log_e2[k] = -28.0;
            }
        }
        self.rng = ec.range();
        self.deemphasis(pcm, n);
        self.loss_count = 0;
        Ok(())
    }

    fn deemphasis(&mut self, pcm: &mut [f32], n: usize) {
        let cc = self.channels;
        for ch in 0..cc {
            let mem = &self.decode_mem[ch];
            let x = &mem[DECODE_BUFFER_SIZE - n..DECODE_BUFFER_SIZE];
            let mut m = self.preemph_mem[ch];
            for (j, &v) in x.iter().enumerate() {
                let tmp = v + 1e-30 + m;
                m = PREEMPH * tmp;
                pcm[j * cc + ch] = tmp * (1.0 / SIG_SCALE);
            }
            self.preemph_mem[ch] = m;
        }
    }

    /// Packet loss concealment (`celt_decode_lost`): pitch-periodic LPC extrapolation of the
    /// decoded history for the first losses, then noise shaped by the band energies.
    pub fn decode_lost(&mut self, pcm: &mut [f32], frame_size: usize) {
        let lm = match Self::lm_for(frame_size) {
            Ok(l) => l,
            Err(_) => {
                pcm.fill(0.0);
                return;
            }
        };
        let n = 120usize << lm;
        if self.loss_count >= 5 || self.start != 0 {
            self.noise_plc(lm);
        } else {
            self.pitch_plc(n);
        }
        self.deemphasis(pcm, n);
        self.loss_count += 1;
    }

    fn noise_plc(&mut self, lm: usize) {
        let cc = self.channels;
        let m = 1usize << lm;
        let n = m * 120;
        let start = self.start;
        let eff_end = self.end.min(NB_EBANDS);
        let use_background = self.loss_count >= 5;
        if !use_background {
            let decay = if self.loss_count == 0 { 1.5 } else { 0.5 };
            for ch in 0..cc {
                for i in start..self.end {
                    self.old_band_e[ch * NB_EBANDS + i] -= decay;
                }
            }
        }
        let mut seed = self.rng;
        for ch in 0..cc {
            let f = &mut self.freq[ch * n..(ch + 1) * n];
            f.fill(0.0);
            for i in start..eff_end {
                let lo = m * eb(i);
                let hi = m * eb(i + 1);
                let x = &mut self.x[..hi - lo];
                for v in x.iter_mut() {
                    seed = bands::lcg_rand(seed);
                    *v = ((seed as i32) >> 20) as f32;
                }
                bands::renormalise_vector(x, 1.0);
                let e = if use_background {
                    self.background_log_e[ch * NB_EBANDS + i]
                } else {
                    self.old_band_e[ch * NB_EBANDS + i]
                };
                let g = (e + E_MEANS[i]).min(32.0).exp2();
                for (j, v) in x.iter().enumerate() {
                    f[lo + j] = v * g;
                }
            }
        }
        self.rng = seed;
        for ch in 0..cc {
            let mem = &mut self.decode_mem[ch];
            mem.copy_within(n..DECODE_BUFFER_SIZE, 0);
            let synth = &mut self.synth[..n + OVERLAP];
            synth[..OVERLAP].fill(0.0);
            self.imdct[lm].backward(&self.freq[ch * n..(ch + 1) * n], 1, synth);
            let base = DECODE_BUFFER_SIZE - n;
            for j in 0..OVERLAP {
                mem[base + j] = synth[j] + mem[DECODE_BUFFER_SIZE + j];
            }
            mem[base + OVERLAP..DECODE_BUFFER_SIZE].copy_from_slice(&synth[OVERLAP..n]);
            mem[DECODE_BUFFER_SIZE..DECODE_BUFFER_SIZE + OVERLAP]
                .copy_from_slice(&synth[n..n + OVERLAP]);
        }
    }

    fn pitch_plc(&mut self, n: usize) {
        const MAX_PERIOD: usize = 1024;
        let ov = OVERLAP;
        let cc = self.channels;
        let (exc, rest) = self.plc_scratch.split_at_mut(MAX_PERIOD);
        let (e, rest) = rest.split_at_mut(MAX_PERIOD + 2 * ov + 4);
        let (pitch_buf, xcorr) = rest.split_at_mut(DECODE_BUFFER_SIZE >> 1);
        let fade;
        if self.loss_count == 0 {
            let chans: [&[f32]; 2] = [
                &self.decode_mem[0][..DECODE_BUFFER_SIZE],
                &self.decode_mem[1][..DECODE_BUFFER_SIZE],
            ];
            plc::pitch_downsample(&chans[..cc], pitch_buf, DECODE_BUFFER_SIZE);
            let p = plc::pitch_search(
                &pitch_buf[720 >> 1..],
                pitch_buf,
                DECODE_BUFFER_SIZE - 720,
                720 - 100,
                xcorr,
            );
            self.last_pitch_index = (720 - p.min(719)).max(1);
            fade = 1.0f32;
        } else {
            fade = 0.8;
        }
        let pitch_index = self.last_pitch_index.clamp(1, MAX_PERIOD);
        let (pp, pg, pt) = (
            self.postfilter_period,
            self.postfilter_gain,
            self.postfilter_tapset,
        );
        for c in 0..cc {
            let mem = &mut self.decode_mem[c];
            let out = DECODE_BUFFER_SIZE - MAX_PERIOD;
            exc.copy_from_slice(&mem[out..out + MAX_PERIOD]);
            let lpc = &mut self.lpc[c];
            if self.loss_count == 0 {
                let mut ac = [0f32; plc::LPC_ORDER + 1];
                plc::autocorr(exc, &mut ac, Some(&WINDOW[..]), plc::LPC_ORDER);
                ac[0] *= 1.0001;
                for (i, a) in ac.iter_mut().enumerate().skip(1) {
                    let f = 0.008 * i as f32;
                    *a -= *a * f * f;
                }
                plc::celt_lpc(lpc, &ac, plc::LPC_ORDER);
            }
            let mut m = [0f32; plc::LPC_ORDER];
            for (i, v) in m.iter_mut().enumerate() {
                *v = mem[out + MAX_PERIOD - 1 - i];
            }
            plc::fir_in_place(exc, &lpc[..], &mut m);
            let period = pitch_index.min(MAX_PERIOD / 2);
            let mut e1 = 1f32;
            let mut e2 = 1f32;
            for i in 0..period {
                let a = exc[MAX_PERIOD - period + i];
                let b = exc[MAX_PERIOD - 2 * period + i];
                e1 += a * a;
                e2 += b * b;
            }
            let mut decay = (e1.min(e2) / e2).sqrt();
            // `offset` goes negative once the extrapolation has repeated the period.
            let mut offset = (MAX_PERIOD - pitch_index) as isize;
            let mut s1 = 0f32;
            let total = n + 2 * ov;
            for i in 0..total {
                while offset + i as isize >= MAX_PERIOD as isize {
                    offset -= pitch_index as isize;
                    decay *= decay;
                }
                let k = (offset + i as isize) as usize;
                e[i] = decay * exc[k] * fade;
                let t = mem[out + k];
                s1 += t * t;
            }
            for (i, v) in m.iter_mut().enumerate() {
                *v = mem[out + MAX_PERIOD - 1 - i];
            }
            plc::iir_in_place(&mut e[..total], &lpc[..], &mut m);
            let s2: f32 = e[..total].iter().map(|v| v * v).sum();
            // Written this way so that NaN also silences the frame.
            #[allow(clippy::neg_cmp_op_on_partial_ord)]
            let explode = !(s1 > 0.2 * s2);
            if explode {
                e[..total].fill(0.0);
            } else if s1 < s2 {
                let ratio = ((s1 + 1.0) / (s2 + 1.0)).sqrt();
                for v in e[..total].iter_mut() {
                    *v *= ratio;
                }
            }
            comb_filter(mem, DECODE_BUFFER_SIZE, pp, pp, ov, pg, pg, pt, pt);
            mem.copy_within(out + n..DECODE_BUFFER_SIZE + ov, out);
            for i in 0..ov / 2 {
                let tmp = WINDOW[i] * e[n + ov - 1 - i] + WINDOW[ov - i - 1] * e[n + i];
                mem[DECODE_BUFFER_SIZE + i] = WINDOW[ov - i - 1] * tmp;
                mem[DECODE_BUFFER_SIZE + ov - i - 1] = WINDOW[i] * tmp;
            }
            mem[DECODE_BUFFER_SIZE - n..DECODE_BUFFER_SIZE].copy_from_slice(&e[..n]);
            // Pre-filter the new overlap (the next frame's post-filter undoes it).
            let t = pp.max(COMBFILTER_MINPERIOD) as usize;
            let g = [
                -pg * COMB_GAINS[pt][0],
                -pg * COMB_GAINS[pt][1],
                -pg * COMB_GAINS[pt][2],
            ];
            for i in 0..ov {
                let k = DECODE_BUFFER_SIZE + i;
                e[i] = mem[k]
                    + g[0] * mem[k - t]
                    + g[1] * (mem[k - t + 1] + mem[k - t - 1])
                    + g[2] * (mem[k - t + 2] + mem[k - t - 2]);
            }
            mem[DECODE_BUFFER_SIZE..DECODE_BUFFER_SIZE + ov].copy_from_slice(&e[..ov]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fast IMDCT against the direct formula of `clt_mdct_backward`'s output.
    #[test]
    fn imdct_matches_direct_definition() {
        for n2 in [120usize, 240, 480, 960] {
            let coefs: Vec<f32> = (0..n2)
                .map(|k| (((k * 7919) % 211) as f32 - 105.0) / 50.0)
                .collect();
            let mut im = Imdct::new(n2);
            let mut out = vec![0.0f32; n2 + OVERLAP];
            im.backward(&coefs, 1, &mut out);
            // Direct: y[n] = Σ X[k] cos(π/N2 (n + 1/2 + N2/2)(k + 1/2)), output = y shifted by
            // (N2 - overlap) / 2 and windowed.
            let d = (n2 - OVERLAP) / 2;
            let n = 2 * n2;
            for p in (0..n2 + OVERLAP).step_by(7) {
                let t = (p + d) as f64;
                let mut s = 0f64;
                for (k, &v) in coefs.iter().enumerate() {
                    s += f64::from(v)
                        * (std::f64::consts::PI / n2 as f64
                            * (t + 0.5 + n2 as f64 / 2.0)
                            * (k as f64 + 0.5))
                            .cos();
                }
                let w = if p < OVERLAP {
                    f64::from(WINDOW[p])
                } else if p >= n2 {
                    f64::from(WINDOW[n2 + OVERLAP - 1 - p])
                } else {
                    1.0
                };
                let _ = n;
                let want = s * w;
                let got = f64::from(out[p]);
                assert!(
                    (got - want).abs() < 1e-2 * (n2 as f64).sqrt(),
                    "n2={n2} p={p}: {got} vs {want}"
                );
            }
        }
    }
}

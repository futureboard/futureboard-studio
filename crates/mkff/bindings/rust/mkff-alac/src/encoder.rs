//! ALAC encoder.

use mkff_audio_core::bits::BitWriter;
use mkff_audio_core::{Encoder, Error, Packet, Result};

use crate::config::{element_layout, AlacConfig, CHANNEL_MAP};
use crate::decoder::{
    BITOFF, ID_CPE, ID_END, MAX_PREFIX, MAX_RUN_BITS, MDENSHIFT, MMULSHIFT, MOFF, N_MAX_MEAN_CLAMP,
    QB, QBSHIFT,
};
use crate::predictor::predict;

/// Predictor settings the encoder writes (Apple's defaults).
const DEN_SHIFT: u32 = 9;
const PB_FACTOR: u32 = 4;
const ORDERS: [usize; 2] = [4, 8];
/// Stereo mixing: `mix_bits` and the `mix_res` values tried (0 = no mixing).
const MIX_BITS: u32 = 2;
const MIX_RES: [i32; 5] = [0, 1, 2, 3, 4];

/// ALAC encoder settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlacEncoderConfig {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channels, 1..=8, in WAV order.
    pub channels: usize,
    /// 16, 20, 24 or 32 bits per sample.
    pub bit_depth: u32,
    /// Samples per packet (default 4096).
    pub frame_length: usize,
}

impl Default for AlacEncoderConfig {
    fn default() -> Self {
        Self {
            sample_rate: 44100,
            channels: 2,
            bit_depth: 16,
            frame_length: 4096,
        }
    }
}

/// Apple Lossless encoder: planar samples in, one ALAC packet per `frame_length` samples out.
///
/// [`Encoder::config`] is the 24-byte `ALACSpecificConfig` magic cookie (wrap it with
/// [`AlacConfig::to_mp4_box`] for an MP4 sample entry). Each element uses the adaptive
/// predictor (orders 4 and 8 tried, initialised by linear prediction), stereo pairs try several
/// mixing weights, 24 / 32-bit audio sends its low 8 / 16 bits verbatim (as Apple's encoder
/// does), and any element that would grow is sent uncompressed.
pub struct AlacEncoder {
    cfg: AlacEncoderConfig,
    pending: Vec<Vec<i32>>,
    quantized: Vec<Vec<i32>>,
    samples: u64,
    bytes: u64,
    max_packet: u32,
    finished: bool,
    s: Scratch,
}

#[derive(Default)]
struct Scratch {
    hi: [Vec<i32>; 2],
    mixed: [Vec<i32>; 2],
    res: Vec<i32>,
    best_res: Vec<i32>,
    windowed: Vec<f64>,
}

impl AlacEncoder {
    /// Create an encoder; fails on out-of-range settings.
    pub fn new(cfg: AlacEncoderConfig) -> Result<Self> {
        if !(1..=8).contains(&cfg.channels) {
            return Err(Error::InvalidArgument(
                "ALAC supports 1..=8 channels".into(),
            ));
        }
        if !matches!(cfg.bit_depth, 16 | 20 | 24 | 32) {
            return Err(Error::InvalidArgument(
                "ALAC bit depth must be 16, 20, 24 or 32".into(),
            ));
        }
        if cfg.frame_length < 16 || cfg.frame_length as u32 > crate::MAX_FRAME_LENGTH {
            return Err(Error::InvalidArgument(
                "ALAC frame length out of range".into(),
            ));
        }
        Ok(Self {
            cfg,
            pending: vec![Vec::new(); cfg.channels],
            quantized: vec![Vec::new(); cfg.channels],
            samples: 0,
            bytes: 0,
            max_packet: 0,
            finished: false,
            s: Scratch::default(),
        })
    }

    /// The magic cookie as known so far (largest packet and average bit rate are final after
    /// [`Encoder::flush`]).
    pub fn alac_config(&self) -> AlacConfig {
        let avg = (self.bytes * 8 * u64::from(self.cfg.sample_rate))
            .checked_div(self.samples)
            .map_or(0, |v| v.min(u64::from(u32::MAX)) as u32);
        AlacConfig {
            frame_length: self.cfg.frame_length as u32,
            bit_depth: self.cfg.bit_depth as u8,
            num_channels: self.cfg.channels as u8,
            max_frame_bytes: self.max_packet,
            avg_bit_rate: avg,
            sample_rate: self.cfg.sample_rate,
            ..AlacConfig::default()
        }
    }

    /// Encode integer samples (one slice per channel, equal lengths, within the bit depth).
    pub fn encode_i32(&mut self, planar: &[&[i32]]) -> Result<Vec<Packet>> {
        self.check(planar.len(), planar.iter().map(|c| c.len()))?;
        let d = self.cfg.bit_depth;
        let (lo, hi) = (-(1i64 << (d - 1)), (1i64 << (d - 1)) - 1);
        if planar
            .iter()
            .any(|c| c.iter().any(|&s| i64::from(s) < lo || i64::from(s) > hi))
        {
            return Err(Error::InvalidArgument(format!(
                "sample outside {d}-bit range"
            )));
        }
        for (p, c) in self.pending.iter_mut().zip(planar) {
            p.extend_from_slice(c);
        }
        Ok(self.drain(false))
    }

    fn check(&self, nch: usize, mut lens: impl Iterator<Item = usize>) -> Result<()> {
        if nch != self.cfg.channels {
            return Err(Error::InvalidArgument(format!(
                "expected {} channels, got {nch}",
                self.cfg.channels
            )));
        }
        let first = lens.next().unwrap_or(0);
        if lens.any(|l| l != first) {
            return Err(Error::InvalidArgument(
                "channel slices differ in length".into(),
            ));
        }
        if self.finished && first > 0 {
            return Err(Error::InvalidArgument(
                "ALAC encoder already flushed".into(),
            ));
        }
        Ok(())
    }

    fn drain(&mut self, all: bool) -> Vec<Packet> {
        let fl = self.cfg.frame_length;
        let avail = self.pending[0].len();
        let mut out = Vec::new();
        let mut off = 0;
        while avail - off >= fl || (all && off < avail) {
            let n = fl.min(avail - off);
            let pending = std::mem::take(&mut self.pending);
            let block: Vec<&[i32]> = pending.iter().map(|c| &c[off..off + n]).collect();
            let data = self.encode_packet(&block);
            self.pending = pending;
            self.bytes += data.len() as u64;
            self.max_packet = self.max_packet.max(data.len() as u32);
            out.push(Packet {
                data,
                pts: self.samples as i64,
                duration: n as u32,
            });
            self.samples += n as u64;
            off += n;
        }
        for p in &mut self.pending {
            p.drain(..off);
        }
        out
    }

    fn encode_packet(&mut self, block: &[&[i32]]) -> Vec<u8> {
        let nch = block.len();
        let map = CHANNEL_MAP[nch - 1];
        let mut w = BitWriter::new();
        let mut ch = 0;
        let mut instances = [0u32; 8];
        for &tag in element_layout(nch) {
            let width = if u32::from(tag) == ID_CPE { 2 } else { 1 };
            let srcs: Vec<&[i32]> = (0..width).map(|c| block[map[ch + c]]).collect();
            let inst = instances[tag as usize];
            instances[tag as usize] += 1;
            self.encode_element(&mut w, u32::from(tag), inst & 15, &srcs);
            ch += width;
        }
        w.write(ID_END, 3);
        w.finish()
    }

    /// Code one SCE / CPE / LFE, compressed or (if smaller) uncompressed.
    fn encode_element(&mut self, w: &mut BitWriter, tag: u32, inst: u32, srcs: &[&[i32]]) {
        let n = srcs[0].len();
        let width = srcs.len();
        let depth = self.cfg.bit_depth;
        let partial = n != self.cfg.frame_length;
        // shifted-off low bytes: Apple's choice first, then one byte less (a win when the low
        // bits are mostly zero); a pair needs depth - shift + 1 <= 32
        let shifts: &[u32] = match (depth, width) {
            (24, _) => &[8, 0],
            (32, 1) => &[16, 8, 0],
            (32, _) => &[16, 8],
            _ => &[0],
        };
        let mut best: Option<BitWriter> = None;
        for &shift in shifts {
            let cw = self.compress(tag, inst, srcs, partial, shift);
            if best.as_ref().is_none_or(|b| cw.bit_len() < b.bit_len()) {
                best = Some(cw);
            }
        }
        let cw = best.expect("one shift candidate");
        let uncompressed = 23 + if partial { 32 } else { 0 } + n * width * depth as usize;
        if cw.bit_len() < uncompressed {
            append_bits(w, cw);
        } else {
            write_header(w, tag, inst, partial, n, 0, true);
            for i in 0..n {
                for src in srcs {
                    w.write_signed(src[i], depth);
                }
            }
        }
    }

    /// The compressed form of an element with `shift` low bits sent verbatim.
    fn compress(
        &mut self,
        tag: u32,
        inst: u32,
        srcs: &[&[i32]],
        partial: bool,
        shift: u32,
    ) -> BitWriter {
        let n = srcs[0].len();
        let width = srcs.len();
        let depth = self.cfg.bit_depth;
        let chan_bits = depth - shift + (width as u32 - 1);
        let mut cw = BitWriter::with_capacity(n * width * 3);
        write_header(&mut cw, tag, inst, partial, n, shift / 8, false);
        let s = &mut self.s;
        for (c, src) in srcs.iter().enumerate() {
            s.hi[c].clear();
            s.hi[c].extend(src.iter().map(|&x| x >> shift));
        }
        // stereo mixing: u = R + ((res * (L - R)) >> bits), v = L - R
        let mix_res = if width == 2 {
            choose_mix(&s.hi[0], &s.hi[1])
        } else {
            0
        };
        if width == 2 && mix_res != 0 {
            let (l, r) = (&s.hi[0], &s.hi[1]);
            s.mixed[0].clear();
            s.mixed[1].clear();
            for (&a, &b) in l.iter().zip(r) {
                let v = a - b;
                s.mixed[0].push(b + ((mix_res * v) >> MIX_BITS));
                s.mixed[1].push(v);
            }
            std::mem::swap(&mut s.mixed, &mut s.hi);
        }
        cw.write(if mix_res != 0 { MIX_BITS } else { 0 }, 8);
        cw.write_signed(mix_res, 8);

        // per channel: pick the predictor order, keep its residuals
        let mut chosen: [(Vec<i16>, Vec<i32>); 2] = Default::default();
        for (c, x) in s.hi.iter().enumerate().take(width) {
            let mut best: Option<(u64, Vec<i16>)> = None;
            for &order in &ORDERS {
                let init = initial_coefs(x, order, &mut s.windowed);
                let mut coefs = init.clone();
                predict(x, &mut s.res, &mut coefs, chan_bits, DEN_SHIFT);
                let cost = residual_cost(&s.res);
                if best.as_ref().is_none_or(|b| cost < b.0) {
                    best = Some((cost, init));
                    std::mem::swap(&mut s.res, &mut s.best_res);
                }
            }
            let (_, init) = best.expect("at least one order");
            chosen[c] = (init, std::mem::take(&mut s.best_res));
        }
        for (init, _) in chosen.iter().take(width) {
            cw.write(0, 4); // mode
            cw.write(DEN_SHIFT, 4);
            cw.write(PB_FACTOR, 3);
            cw.write(init.len() as u32, 5);
            for &k in init {
                cw.write_signed(i32::from(k), 16);
            }
        }
        if shift > 0 {
            let mask = (1u32 << shift) - 1;
            for i in 0..n {
                for src in srcs {
                    cw.write(src[i] as u32 & mask, shift);
                }
            }
        }
        let pb = 40 * PB_FACTOR / 4;
        for (_, res) in chosen.iter().take(width) {
            encode_residuals(&mut cw, res, pb, 10, 14, chan_bits);
        }
        // hand the residual buffers back for reuse
        s.best_res = std::mem::take(&mut chosen[0].1);
        cw
    }
}

/// Element header: tag, instance, 12 unused bits, partial flag, shifted bytes, escape flag and
/// (for a partial frame) the sample count.
fn write_header(
    w: &mut BitWriter,
    tag: u32,
    inst: u32,
    partial: bool,
    n: usize,
    shifted: u32,
    escape: bool,
) {
    w.write(tag, 3);
    w.write(inst, 4);
    w.write(0, 12);
    w.write(u32::from(partial), 1);
    w.write(shifted, 2);
    w.write(u32::from(escape), 1);
    if partial {
        w.write(n as u32, 32);
    }
}

impl Encoder for AlacEncoder {
    /// Quantize f32 input (full scale = ±1.0) to the bit depth with clipping and encode it.
    fn encode(&mut self, input: &[&[f32]]) -> Result<Vec<Packet>> {
        self.check(input.len(), input.iter().map(|c| c.len()))?;
        let scale = (1u64 << (self.cfg.bit_depth - 1)) as f64;
        for (q, c) in self.quantized.iter_mut().zip(input) {
            q.clear();
            q.extend(c.iter().map(|&x| {
                let v = (f64::from(x) * scale).round();
                if v.is_nan() {
                    0
                } else {
                    v.clamp(-scale, scale - 1.0) as i32
                }
            }));
        }
        for (p, q) in self.pending.iter_mut().zip(&self.quantized) {
            p.extend_from_slice(q);
        }
        Ok(self.drain(false))
    }

    /// Encode what is buffered as a final partial packet (no padding needed).
    fn flush(&mut self) -> Result<Vec<Packet>> {
        let out = self.drain(true);
        if out
            .last()
            .is_some_and(|p| p.duration as usize != self.cfg.frame_length)
        {
            self.finished = true;
        }
        Ok(out)
    }

    fn sample_rate(&self) -> u32 {
        self.cfg.sample_rate
    }

    fn channels(&self) -> usize {
        self.cfg.channels
    }

    fn frame_size(&self) -> usize {
        self.cfg.frame_length
    }

    fn priming(&self) -> u32 {
        0
    }

    /// The 24-byte `ALACSpecificConfig` magic cookie.
    fn config(&self) -> Vec<u8> {
        self.alac_config().to_bytes().to_vec()
    }
}

/// Copy the bits of `src` onto `dst`.
fn append_bits(dst: &mut BitWriter, src: BitWriter) {
    let bits = src.bit_len();
    let bytes = src.finish();
    for &b in &bytes[..bits / 8] {
        dst.write(u32::from(b), 8);
    }
    let rem = (bits % 8) as u32;
    if rem > 0 {
        dst.write(u32::from(bytes[bits / 8]) >> (8 - rem), rem);
    }
}

/// Mixing weight with the smallest first-difference energy of the mixed pair.
fn choose_mix(l: &[i32], r: &[i32]) -> i32 {
    let mut best = (u64::MAX, 0);
    for &res in &MIX_RES {
        let mut cost = 0u64;
        let (mut pu, mut pv) = (0i64, 0i64);
        for (&a, &b) in l.iter().zip(r) {
            let (u, v) = if res == 0 {
                (i64::from(a), i64::from(b))
            } else {
                let v = i64::from(a) - i64::from(b);
                (i64::from(b) + ((i64::from(res) * v) >> MIX_BITS), v)
            };
            cost += (u - pu).unsigned_abs() + (v - pv).unsigned_abs();
            pu = u;
            pv = v;
        }
        if cost < best.0 {
            best = (cost, res);
        }
    }
    best.1
}

/// Rough coded size of residuals (sum of magnitudes stands in for the adaptive Rice cost).
fn residual_cost(res: &[i32]) -> u64 {
    res.iter().map(|r| u64::from(r.unsigned_abs())).sum()
}

/// Initial predictor coefficients from Levinson-Durbin on a windowed copy of `x`, in units of
/// `2^-DEN_SHIFT`.
fn initial_coefs(x: &[i32], order: usize, windowed: &mut Vec<f64>) -> Vec<i16> {
    let n = x.len();
    if n <= order * 2 {
        return Vec::new();
    }
    windowed.clear();
    windowed.extend(x.iter().enumerate().map(|(i, &s)| {
        let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * (i as f64 + 0.5) / n as f64).cos();
        f64::from(s) * w
    }));
    let mut ac = [0.0f64; 33];
    for (lag, a) in ac.iter_mut().enumerate().take(order + 1) {
        *a = windowed[lag..]
            .iter()
            .zip(windowed.iter())
            .map(|(p, q)| p * q)
            .sum();
    }
    if ac[0] <= 0.0 {
        return vec![0; order];
    }
    let mut a = [0.0f64; 32];
    let mut e = ac[0] * (1.0 + 1e-9);
    for m in 0..order {
        let mut acc = ac[m + 1];
        for j in 0..m {
            acc -= a[j] * ac[m - j];
        }
        let k = acc / e;
        if !k.is_finite() || k.abs() >= 1.0 {
            break;
        }
        let prev = a;
        a[m] = k;
        for j in 0..m {
            a[j] = prev[j] - k * prev[m - 1 - j];
        }
        e *= 1.0 - k * k;
    }
    let scale = f64::from(1u32 << DEN_SHIFT);
    a[..order]
        .iter()
        .map(|c| (c * scale).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

/// Write one adaptive-Golomb value (the inverse of the decoder's reader).
#[inline]
fn write_golomb(w: &mut BitWriter, val: u32, k: u32, m: u32, escape_bits: u32) {
    let pre = match (k, m) {
        (0, _) | (_, 0) => {
            if val == 0 {
                0
            } else {
                MAX_PREFIX
            }
        }
        (1, _) => val.min(MAX_PREFIX),
        _ => (val / m).min(MAX_PREFIX),
    };
    if pre >= MAX_PREFIX {
        w.write((1 << MAX_PREFIX) - 1, MAX_PREFIX);
        w.write(val, escape_bits);
        return;
    }
    // `pre` ones and the terminating zero
    w.write(((1u32 << pre) - 1) << 1, pre + 1);
    if k >= 2 {
        let rem = val - pre * m;
        if rem == 0 {
            w.write(0, k - 1);
        } else {
            w.write(rem + 1, k);
        }
    }
}

/// Adaptive Golomb-Rice coding of one channel's residuals (the decoder's `decode_residuals`
/// in reverse, with the same history updates and zero runs).
fn encode_residuals(w: &mut BitWriter, res: &[i32], pb: u32, mb0: u32, kb: u32, max_bits: u32) {
    let n = res.len();
    let wb = (1u32 << kb) - 1;
    let mut mb = mb0;
    let mut zmode = 0u32;
    let mut c = 0;
    while c < n {
        let k = (31 - ((mb >> QBSHIFT) + 3).leading_zeros()).min(kb);
        let r = i64::from(res[c]);
        let nd = if r >= 0 { 2 * r } else { -2 * r - 1 } as u64;
        let v = (nd - u64::from(zmode)) as u32;
        write_golomb(w, v, k, (1u32 << k) - 1, max_bits);
        c += 1;
        mb = pb
            .wrapping_mul(v.wrapping_add(zmode))
            .wrapping_add(mb)
            .wrapping_sub(pb.wrapping_mul(mb) >> QBSHIFT);
        if v > N_MAX_MEAN_CLAMP {
            mb = N_MAX_MEAN_CLAMP;
        }
        zmode = 0;
        if (mb << MMULSHIFT) < QB && c < n {
            zmode = 1;
            let k = (mb.leading_zeros() as i32 - BITOFF
                + (mb.wrapping_add(MOFF) >> MDENSHIFT) as i32)
                .clamp(0, 31) as u32;
            let run = res[c..].iter().take(65535).take_while(|&&r| r == 0).count();
            write_golomb(w, run as u32, k, ((1u32 << k) - 1) & wb, MAX_RUN_BITS);
            c += run;
            if run >= 65535 {
                zmode = 0;
            }
            mb = 0;
        }
    }
}

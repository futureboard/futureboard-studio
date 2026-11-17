//! FLAC encoder.

use mkff_audio_core::bits::BitWriter;
use mkff_audio_core::crc::{crc16, crc8};
use mkff_audio_core::{Encoder, Error, Packet, Result};

use crate::frame::{sample_rate_code, write_coded_number, ChannelAssignment};
use crate::lpc::{self, MAX_ORDER};
use crate::md5::Md5;
use crate::StreamInfo;

/// FLAC encoder settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlacEncoderConfig {
    /// Sample rate in Hz (1..=1048575).
    pub sample_rate: u32,
    /// Channels, 1..=8, in WAV order (FLAC's own channel order for 1..=8 channels).
    pub channels: usize,
    /// Bits per sample, 4..=32 (16 and 24 are the common ones). Inputs to
    /// [`FlacEncoder::encode_i32`] must lie in the signed range of this many bits.
    pub bits_per_sample: u32,
    /// Samples per frame, 16..=65535 (default 4096). Only the last frame may be shorter.
    pub block_size: usize,
    /// Compression effort 0..=8, modelled on the reference encoder's levels:
    ///
    /// | level | predictors | max LPC order | stereo decorrelation | max partition order |
    /// |-------|------------|---------------|----------------------|---------------------|
    /// | 0 | fixed | - | none | 3 |
    /// | 1 | fixed | - | estimated | 3 |
    /// | 2 | fixed | - | exhaustive | 3 |
    /// | 3 | fixed + LPC | 6 | none | 4 |
    /// | 4 | fixed + LPC | 8 | estimated | 4 |
    /// | 5 | fixed + LPC | 8 | exhaustive | 5 |
    /// | 6 | fixed + LPC, 3 windows | 8 | exhaustive | 6 |
    /// | 7 | fixed + LPC, 3 windows, 2 orders tried | 12 | exhaustive | 6 |
    /// | 8 | fixed + LPC, 6 windows, 3 orders and all precisions tried | 12 | exhaustive | 6 |
    pub level: u8,
}

impl Default for FlacEncoderConfig {
    fn default() -> Self {
        Self {
            sample_rate: 44100,
            channels: 2,
            bits_per_sample: 16,
            block_size: 4096,
            level: 5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stereo {
    Independent,
    Estimated,
    Exhaustive,
}

/// What a level turns on.
#[derive(Clone, Copy, Debug)]
struct Params {
    max_lpc: usize,
    stereo: Stereo,
    max_porder: u32,
    /// 0 = no LPC, 1 = one Tukey(0.5), 2 = + halves, 3 = + thirds.
    subdivide: u8,
    /// Orders per window evaluated exactly (by estimated size); 0 = all.
    orders_tried: usize,
    precision_search: bool,
}

impl Params {
    fn for_level(level: u8) -> Self {
        let p = |max_lpc, stereo, max_porder, subdivide, orders_tried, precision_search| Params {
            max_lpc,
            stereo,
            max_porder,
            subdivide,
            orders_tried,
            precision_search,
        };
        match level {
            0 => p(0, Stereo::Independent, 3, 0, 1, false),
            1 => p(0, Stereo::Estimated, 3, 0, 1, false),
            2 => p(0, Stereo::Exhaustive, 3, 0, 1, false),
            3 => p(6, Stereo::Independent, 4, 1, 1, false),
            4 => p(8, Stereo::Estimated, 4, 1, 1, false),
            5 => p(8, Stereo::Exhaustive, 5, 1, 1, false),
            6 => p(8, Stereo::Exhaustive, 6, 2, 1, false),
            7 => p(12, Stereo::Exhaustive, 6, 2, 2, false),
            _ => p(12, Stereo::Exhaustive, 6, 3, 3, true),
        }
    }
}

/// How one subframe is coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Constant,
    Verbatim,
    Fixed(usize),
    Lpc {
        order: usize,
        precision: u32,
        shift: u32,
        coefs: [i32; MAX_ORDER],
    },
}

/// The chosen coding of one candidate channel signal.
#[derive(Clone, Debug)]
struct Subframe {
    kind: Kind,
    wasted: u32,
    /// Bits per sample of `signal` (after removing wasted bits).
    bps: u32,
    /// Estimated size in bits.
    bits: u64,
    porder: u32,
    rice2: bool,
    params: Vec<u32>,
    /// Samples with wasted bits shifted out.
    signal: Vec<i32>,
    /// Residual of samples `order..n`.
    residual: Vec<i32>,
}

impl Default for Subframe {
    fn default() -> Self {
        Self {
            kind: Kind::Verbatim,
            wasted: 0,
            bps: 0,
            bits: 0,
            porder: 0,
            rice2: false,
            params: Vec::new(),
            signal: Vec::new(),
            residual: Vec::new(),
        }
    }
}

impl Default for Params {
    fn default() -> Self {
        Params::for_level(5)
    }
}

/// Scratch space for subframe analysis (reused across frames: no per-sample allocation).
#[derive(Default)]
struct Analyzer {
    params: Params,
    windows: Vec<Vec<f64>>,
    window_len: usize,
    windowed: Vec<f64>,
    tmp_res: Vec<i32>,
    tmp_params: Vec<u32>,
    sums: Vec<u64>,
}

/// FLAC encoder: planar samples in, one packet per FLAC frame out.
///
/// The stream header for a native `.flac` file is [`StreamInfo::to_stream_header`] of
/// [`FlacEncoder::stream_info`] after [`Encoder::flush`]; containers store
/// [`Encoder::config`] (the 34-byte `STREAMINFO` body).
///
/// ```
/// use mkff_audio_core::Encoder;
/// use mkff_flac::{FlacEncoder, FlacEncoderConfig};
/// # fn main() -> mkff_audio_core::Result<()> {
/// let cfg = FlacEncoderConfig { channels: 1, ..Default::default() };
/// let mut enc = FlacEncoder::new(cfg)?;
/// let tone: Vec<i32> = (0..10_000).map(|i| ((i as f32 * 0.05).sin() * 8000.0) as i32).collect();
/// let mut frames = enc.encode_i32(&[&tone])?;
/// frames.extend(enc.flush()?);
/// let info = enc.stream_info();
/// assert_eq!(info.total_samples, 10_000);
/// # Ok(()) }
/// ```
pub struct FlacEncoder {
    cfg: FlacEncoderConfig,
    params: Params,
    pending: Vec<Vec<i32>>,
    analyzer: Analyzer,
    slots: Vec<Subframe>,
    mid: Vec<i32>,
    side: Vec<i32>,
    quantized: Vec<Vec<i32>>,
    md5: Md5,
    md5_buf: Vec<u8>,
    digest: Option<[u8; 16]>,
    frame_number: u64,
    samples: u64,
    min_frame: u32,
    max_frame: u32,
    /// A short (final) frame has been written: no more input is accepted.
    finished: bool,
}

impl FlacEncoder {
    /// Create an encoder; fails on out-of-range settings.
    pub fn new(cfg: FlacEncoderConfig) -> Result<Self> {
        if !(1..=8).contains(&cfg.channels) {
            return Err(Error::InvalidArgument(
                "FLAC supports 1..=8 channels".into(),
            ));
        }
        if !(4..=32).contains(&cfg.bits_per_sample) {
            return Err(Error::InvalidArgument(
                "FLAC supports 4..=32 bits per sample".into(),
            ));
        }
        if !(1..=0xF_FFFF).contains(&cfg.sample_rate) {
            return Err(Error::InvalidArgument(
                "FLAC sample rate must be 1..=1048575 Hz".into(),
            ));
        }
        if !(16..=65535).contains(&cfg.block_size) {
            return Err(Error::InvalidArgument(
                "FLAC block size must be 16..=65535".into(),
            ));
        }
        if cfg.level > 8 {
            return Err(Error::InvalidArgument("FLAC level must be 0..=8".into()));
        }
        let params = Params::for_level(cfg.level);
        Ok(Self {
            cfg,
            params,
            pending: vec![Vec::with_capacity(cfg.block_size * 2); cfg.channels],
            analyzer: Analyzer {
                params,
                ..Default::default()
            },
            slots: vec![Subframe::default(); cfg.channels.max(4)],
            mid: Vec::new(),
            side: Vec::new(),
            quantized: vec![Vec::new(); cfg.channels],
            md5: Md5::new(),
            md5_buf: Vec::new(),
            digest: None,
            frame_number: 0,
            samples: 0,
            min_frame: 0,
            max_frame: 0,
            finished: false,
        })
    }

    /// The configuration in use.
    pub fn encoder_config(&self) -> &FlacEncoderConfig {
        &self.cfg
    }

    /// `STREAMINFO` as known so far. After [`Encoder::flush`] it is final: total samples,
    /// smallest / largest frame sizes and the MD5 of all input samples.
    pub fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            min_block: self.cfg.block_size as u16,
            max_block: self.cfg.block_size as u16,
            min_frame: self.min_frame,
            max_frame: self.max_frame,
            sample_rate: self.cfg.sample_rate,
            channels: self.cfg.channels as u8,
            bits_per_sample: self.cfg.bits_per_sample as u8,
            total_samples: self.samples + self.pending[0].len() as u64,
            md5: self.digest.unwrap_or([0; 16]),
        }
    }

    /// Encode integer samples (one slice per channel, equal lengths, each sample within the
    /// configured bit depth). Returns the frames completed so far.
    pub fn encode_i32(&mut self, planar: &[&[i32]]) -> Result<Vec<Packet>> {
        self.check_input(planar.len(), planar.iter().map(|c| c.len()))?;
        let bps = self.cfg.bits_per_sample;
        let (lo, hi) = (-(1i64 << (bps - 1)), (1i64 << (bps - 1)) - 1);
        for c in planar {
            if c.iter().any(|&s| i64::from(s) < lo || i64::from(s) > hi) {
                return Err(Error::InvalidArgument(format!(
                    "sample outside {bps}-bit range"
                )));
            }
        }
        for (p, c) in self.pending.iter_mut().zip(planar) {
            p.extend_from_slice(c);
        }
        self.drain_full_blocks()
    }

    fn check_input(&self, nch: usize, mut lens: impl Iterator<Item = usize>) -> Result<()> {
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
                "FLAC encoder already flushed".into(),
            ));
        }
        Ok(())
    }

    fn drain_full_blocks(&mut self) -> Result<Vec<Packet>> {
        let bs = self.cfg.block_size;
        let avail = self.pending[0].len();
        let mut packets = Vec::new();
        let mut off = 0;
        while avail - off >= bs {
            packets.push(self.encode_block(off, bs));
            off += bs;
        }
        if off > 0 {
            for p in &mut self.pending {
                p.drain(..off);
            }
        }
        Ok(packets)
    }

    /// Encode `pending[..][off..off + n]` as one frame.
    fn encode_block(&mut self, off: usize, n: usize) -> Packet {
        self.update_md5(off, n);
        let pending = std::mem::take(&mut self.pending);
        let block: Vec<&[i32]> = pending.iter().map(|c| &c[off..off + n]).collect();
        let data = self.encode_frame(&block);
        drop(block);
        self.pending = pending;
        let len = data.len() as u32;
        self.min_frame = if self.min_frame == 0 {
            len
        } else {
            self.min_frame.min(len)
        };
        self.max_frame = self.max_frame.max(len);
        let pkt = Packet {
            data,
            pts: self.samples as i64,
            duration: n as u32,
        };
        self.samples += n as u64;
        self.frame_number += 1;
        pkt
    }

    fn update_md5(&mut self, off: usize, n: usize) {
        let bytes = self.cfg.bits_per_sample.div_ceil(8) as usize;
        self.md5_buf.clear();
        self.md5_buf.reserve(n * bytes * self.cfg.channels);
        for i in off..off + n {
            for c in &self.pending {
                self.md5_buf.extend_from_slice(&c[i].to_le_bytes()[..bytes]);
            }
        }
        self.md5.update(&self.md5_buf);
    }

    /// Code one block (all channels) as a complete frame.
    fn encode_frame(&mut self, block: &[&[i32]]) -> Vec<u8> {
        let n = block[0].len();
        let bps = self.cfg.bits_per_sample;
        self.analyzer.prepare(n);
        let assignment =
            if block.len() == 2 && bps < 32 && self.params.stereo != Stereo::Independent {
                self.mid.clear();
                self.side.clear();
                for (&l, &r) in block[0].iter().zip(block[1]) {
                    self.mid.push(((i64::from(l) + i64::from(r)) >> 1) as i32);
                    self.side.push(l - r);
                }
                let sources: [(&[i32], u32); 4] = [
                    (block[0], bps),
                    (block[1], bps),
                    (&self.mid, bps),
                    (&self.side, bps + 1),
                ];
                let chosen = if self.params.stereo == Stereo::Exhaustive {
                    for (slot, (src, b)) in self.slots.iter_mut().zip(sources) {
                        self.analyzer.analyze(src, b, slot);
                    }
                    let c: Vec<u64> = self.slots[..4].iter().map(|s| s.bits).collect();
                    best_assignment([c[0] + c[1], c[0] + c[3], c[3] + c[1], c[2] + c[3]])
                } else {
                    let e: Vec<u64> = sources
                        .iter()
                        .map(|(s, b)| estimate_fixed_bits(s, *b))
                        .collect();
                    let a = best_assignment([e[0] + e[1], e[0] + e[3], e[3] + e[1], e[2] + e[3]]);
                    for i in assignment_slots(a) {
                        let (src, b) = sources[i];
                        self.analyzer.analyze(src, b, &mut self.slots[i]);
                    }
                    a
                };
                chosen
            } else {
                for (slot, src) in self.slots.iter_mut().zip(block) {
                    self.analyzer.analyze(src, bps, slot);
                }
                ChannelAssignment::Independent(block.len() as u8)
            };
        let slot_ids: Vec<usize> = match assignment {
            ChannelAssignment::Independent(c) => (0..c as usize).collect(),
            a => assignment_slots(a).to_vec(),
        };

        let est: u64 = slot_ids.iter().map(|&i| self.slots[i].bits).sum();
        // the estimate is close; verbatim bounds it anyway
        let cap = (est / 8).min(n as u64 * slot_ids.len() as u64 * 5) as usize + 64;
        let mut w = BitWriter::with_capacity(cap);
        let header = self.frame_header(n, assignment);
        w.write_bytes(&header);
        for &i in &slot_ids {
            write_subframe(&mut w, &self.slots[i], n);
        }
        w.byte_align();
        let crc = crc16(0, w.bytes());
        w.write(u32::from(crc), 16);
        w.finish()
    }

    fn frame_header(&self, n: usize, assignment: ChannelAssignment) -> Vec<u8> {
        let mut h = Vec::with_capacity(16);
        h.extend_from_slice(&[0xFF, 0xF8]);
        let bs_code: u8 = match n {
            192 => 1,
            576 | 1152 | 2304 | 4608 => 2 + (n / 576).trailing_zeros() as u8,
            256 | 512 | 1024 | 2048 | 4096 | 8192 | 16384 | 32768 => {
                8 + (n / 256).trailing_zeros() as u8
            }
            _ if n <= 256 => 6,
            _ => 7,
        };
        let rate = self.cfg.sample_rate;
        let sr_code = match sample_rate_code(rate) {
            Some(c) => c,
            None if rate.is_multiple_of(1000) && rate / 1000 <= 255 => 12,
            None if rate <= 65535 => 13,
            None if rate.is_multiple_of(10) && rate / 10 <= 65535 => 14,
            None => 0,
        };
        let bps_code = match self.cfg.bits_per_sample {
            8 => 1,
            12 => 2,
            16 => 4,
            20 => 5,
            24 => 6,
            32 => 7,
            _ => 0,
        };
        h.push((bs_code << 4) | sr_code);
        h.push((assignment.code() << 4) | (bps_code << 1));
        let mut num = [0u8; 7];
        let len = write_coded_number(self.frame_number, &mut num);
        h.extend_from_slice(&num[..len]);
        match bs_code {
            6 => h.push((n - 1) as u8),
            7 => h.extend_from_slice(&((n - 1) as u16).to_be_bytes()),
            _ => {}
        }
        match sr_code {
            12 => h.push((rate / 1000) as u8),
            13 => h.extend_from_slice(&(rate as u16).to_be_bytes()),
            14 => h.extend_from_slice(&((rate / 10) as u16).to_be_bytes()),
            _ => {}
        }
        h.push(crc8(&h));
        h
    }

    fn quantize_input(&mut self, input: &[&[f32]]) {
        let scale = (1u64 << (self.cfg.bits_per_sample - 1)) as f64;
        let (lo, hi) = (-scale, scale - 1.0);
        for (q, c) in self.quantized.iter_mut().zip(input) {
            q.clear();
            q.extend(c.iter().map(|&x| {
                let v = (f64::from(x) * scale).round();
                if v.is_nan() {
                    0
                } else {
                    v.clamp(lo, hi) as i32
                }
            }));
        }
    }
}

impl Encoder for FlacEncoder {
    /// Quantize f32 input (full scale = ±1.0) to the configured bit depth with clipping, then
    /// encode it like [`FlacEncoder::encode_i32`].
    fn encode(&mut self, input: &[&[f32]]) -> Result<Vec<Packet>> {
        self.check_input(input.len(), input.iter().map(|c| c.len()))?;
        self.quantize_input(input);
        let quantized = std::mem::take(&mut self.quantized);
        for (p, c) in self.pending.iter_mut().zip(&quantized) {
            p.extend_from_slice(c);
        }
        self.quantized = quantized;
        self.drain_full_blocks()
    }

    /// Encode buffered samples as a final, shorter frame (FLAC needs no padding) and finalize
    /// [`FlacEncoder::stream_info`]. Further input is rejected once a short frame was written.
    fn flush(&mut self) -> Result<Vec<Packet>> {
        let mut out = self.drain_full_blocks()?;
        let rest = self.pending[0].len();
        if rest > 0 {
            out.push(self.encode_block(0, rest));
            for p in &mut self.pending {
                p.clear();
            }
            self.finished = true;
        }
        self.digest = Some(self.md5.clone().finish());
        Ok(out)
    }

    fn sample_rate(&self) -> u32 {
        self.cfg.sample_rate
    }

    fn channels(&self) -> usize {
        self.cfg.channels
    }

    fn frame_size(&self) -> usize {
        self.cfg.block_size
    }

    fn priming(&self) -> u32 {
        0
    }

    /// The 34-byte `STREAMINFO` body as known so far (final after `flush`).
    fn config(&self) -> Vec<u8> {
        self.stream_info().to_bytes().to_vec()
    }
}

/// Cheapest of [independent, left-side, side-right, mid-side] costs.
fn best_assignment(costs: [u64; 4]) -> ChannelAssignment {
    let modes = [
        ChannelAssignment::Independent(2),
        ChannelAssignment::LeftSide,
        ChannelAssignment::SideRight,
        ChannelAssignment::MidSide,
    ];
    let mut best = 0;
    for i in 1..4 {
        if costs[i] < costs[best] {
            best = i;
        }
    }
    modes[best]
}

/// Analysis slots (0 = L, 1 = R, 2 = M, 3 = S) a stereo assignment codes, in frame order.
fn assignment_slots(a: ChannelAssignment) -> [usize; 2] {
    match a {
        ChannelAssignment::LeftSide => [0, 3],
        ChannelAssignment::SideRight => [3, 1],
        ChannelAssignment::MidSide => [2, 3],
        ChannelAssignment::Independent(_) => [0, 1],
    }
}

/// Fixed-predictor residual magnitude sums for orders 0..=4 over samples 4..n.
fn fixed_sums(x: &[i32]) -> [u64; 5] {
    let mut sums = [0u64; 5];
    if x.len() < 5 {
        return sums;
    }
    let mut p1 = i64::from(x[3]);
    let mut d1 = p1 - i64::from(x[2]);
    let d1_prev = i64::from(x[2]) - i64::from(x[1]);
    let mut d2 = d1 - d1_prev;
    let d2_prev = d1_prev - (i64::from(x[1]) - i64::from(x[0]));
    let mut d3 = d2 - d2_prev;
    for &s in &x[4..] {
        let e0 = i64::from(s);
        let e1 = e0 - p1;
        let e2 = e1 - d1;
        let e3 = e2 - d2;
        let e4 = e3 - d3;
        sums[0] += e0.unsigned_abs();
        sums[1] += e1.unsigned_abs();
        sums[2] += e2.unsigned_abs();
        sums[3] += e3.unsigned_abs();
        sums[4] += e4.unsigned_abs();
        p1 = e0;
        d1 = e1;
        d2 = e2;
        d3 = e3;
    }
    sums
}

/// Rough size of a channel coded with its best fixed predictor (stereo mode estimation).
fn estimate_fixed_bits(x: &[i32], bps: u32) -> u64 {
    let n = x.len().max(1) as u64;
    let sum = fixed_sums(x).into_iter().min().unwrap_or(0);
    if x.len() < 5 {
        return n * u64::from(bps);
    }
    let mean = (sum / n).max(1);
    n * u64::from(65 - mean.leading_zeros())
}

/// Best Rice parameter and estimated bits for `cnt` values summing (zig-zag) to `sum`.
#[inline]
fn rice_cost(sum: u64, cnt: u64) -> (u64, u32) {
    if cnt == 0 {
        return (0, 0);
    }
    let mean = sum / cnt;
    let k0 = (64 - mean.leading_zeros()).min(30);
    let mut best = (u64::MAX, 0);
    for k in k0.saturating_sub(1)..=(k0 + 1).min(30) {
        // sum >> k under-counts the quotients by about cnt / 2 when k > 0
        let q = (sum >> k).saturating_sub(if k > 0 { cnt / 2 } else { 0 });
        let bits = cnt * u64::from(k + 1) + q;
        if bits < best.0 {
            best = (bits, k);
        }
    }
    best
}

#[inline]
fn zigzag(r: i32) -> u32 {
    ((r << 1) ^ (r >> 31)) as u32
}

impl Analyzer {
    /// Recompute the apodization windows for block length `n` if needed.
    fn prepare(&mut self, n: usize) {
        let p = self.params;
        if p.subdivide == 0 || self.window_len == n {
            return;
        }
        self.window_len = n;
        let mut specs: Vec<(usize, usize, f64)> = vec![(0, n, 0.5)];
        for parts in 2..=usize::from(p.subdivide) {
            let len = n / parts;
            if len < 32 {
                break;
            }
            for i in 0..parts {
                let start = i * len;
                let l = if i + 1 == parts { n - start } else { len };
                specs.push((start, l, 0.25));
            }
        }
        self.windows.resize_with(specs.len(), Vec::new);
        self.windows.truncate(specs.len());
        for (w, (start, len, taper)) in self.windows.iter_mut().zip(specs) {
            lpc::tukey_into(w, n, start, len, taper);
        }
    }

    /// Find the cheapest coding of `input` (samples of `bps` bits) into `out`.
    fn analyze(&mut self, input: &[i32], bps: u32, out: &mut Subframe) {
        let mut signal = std::mem::take(&mut out.signal);
        self.analyze_into(input, bps, &mut signal, out);
        out.signal = signal;
    }

    /// [`Analyzer::analyze`] with the (wasted-bits-shifted) signal kept outside `out`.
    fn analyze_into(&mut self, input: &[i32], bps: u32, signal: &mut Vec<i32>, out: &mut Subframe) {
        let n = input.len();
        signal.clear();
        out.wasted = 0;
        out.bps = bps;
        out.porder = 0;
        out.rice2 = false;
        out.params.clear();
        out.residual.clear();
        if input.iter().all(|&s| s == input[0]) {
            out.kind = Kind::Constant;
            signal.push(input[0]);
            out.bits = 8 + u64::from(bps);
            return;
        }
        let or = input.iter().fold(0i32, |a, &s| a | s);
        let wasted = or.trailing_zeros().min(bps - 1);
        out.wasted = wasted;
        signal.extend(input.iter().map(|&s| s >> wasted));
        let bps = bps - wasted;
        out.bps = bps;
        let head = 8 + u64::from(wasted);
        out.kind = Kind::Verbatim;
        out.bits = head + n as u64 * u64::from(bps);

        let max_porder = self.params.max_porder;
        let x = &signal[..];
        // fixed predictors: pick the order with the smallest residual magnitude
        if n > 4 {
            let sums = fixed_sums(x);
            let order = (0..5).min_by_key(|&o| sums[o]).unwrap_or(0);
            if fixed_residual(x, order, &mut self.tmp_res) {
                let (rb, po, r2) = rice_search(
                    &self.tmp_res,
                    n,
                    order,
                    max_porder,
                    &mut self.sums,
                    &mut self.tmp_params,
                );
                let bits = head + order as u64 * u64::from(bps) + 6 + rb;
                if bits < out.bits {
                    out.bits = bits;
                    out.kind = Kind::Fixed(order);
                    out.porder = po;
                    out.rice2 = r2;
                    std::mem::swap(&mut out.params, &mut self.tmp_params);
                    std::mem::swap(&mut out.residual, &mut self.tmp_res);
                }
            }
        }

        let max_order = self.params.max_lpc.min(n / 4);
        if max_order == 0 || self.windows.is_empty() {
            return;
        }
        // coefficient precision: the coefficients cost order * precision bits per subframe,
        // which only pays off over longer blocks
        let base_precision = match n {
            0..=384 => 12,
            385..=1152 => 14,
            _ => 15,
        };
        let mut lpc = [[0.0f64; MAX_ORDER]; MAX_ORDER];
        let mut err = [0.0f64; MAX_ORDER];
        let mut ac = [0.0f64; MAX_ORDER + 1];
        // best LPC candidate so far: (window, order) and its real-valued coefficients
        let mut best_lpc: Option<[f64; MAX_ORDER]> = None;
        let mut best_order = 0;
        let mut q = [0i32; MAX_ORDER];
        let mut cand: Vec<(f64, usize)> = Vec::with_capacity(MAX_ORDER);
        for wi in 0..self.windows.len() {
            self.windowed.clear();
            self.windowed.extend(
                x.iter()
                    .zip(&self.windows[wi])
                    .map(|(&s, &w)| f64::from(s) * w),
            );
            lpc::autocorrelation(&self.windowed, max_order, &mut ac);
            let got = lpc::levinson(&ac, max_order, &mut lpc, &mut err);
            if got == 0 {
                continue;
            }
            cand.clear();
            for o in 1..=got {
                let var = err[o - 1] / n as f64;
                let per_sample = if var > 0.0 {
                    (0.5 * var.log2() + 1.0).max(1.0)
                } else {
                    1.0
                };
                let est = per_sample * n as f64 + (o as f64) * f64::from(bps + base_precision);
                cand.push((est, o));
            }
            cand.sort_by(|a, b| a.0.total_cmp(&b.0));
            let tried = match self.params.orders_tried {
                0 => cand.len(),
                t => t.min(cand.len()),
            };
            for &(_, order) in &cand[..tried] {
                let coefs = &lpc[order - 1][..order];
                if self
                    .try_lpc(x, bps, head, coefs, base_precision, &mut q, out)
                    .is_some()
                {
                    let mut c = [0.0; MAX_ORDER];
                    c[..order].copy_from_slice(coefs);
                    best_lpc = Some(c);
                    best_order = order;
                }
            }
        }
        if self.params.precision_search {
            if let Some(c) = best_lpc {
                for precision in (base_precision + 1..=15).chain(base_precision - 3..base_precision)
                {
                    self.try_lpc(x, bps, head, &c[..best_order], precision, &mut q, out);
                }
            }
        }
    }

    /// Quantize `coefs` at `precision`, code the residual and keep it in `out` if smaller.
    /// Returns the size if it was kept.
    #[allow(clippy::too_many_arguments)]
    fn try_lpc(
        &mut self,
        x: &[i32],
        bps: u32,
        head: u64,
        coefs: &[f64],
        precision: u32,
        q: &mut [i32; MAX_ORDER],
        out: &mut Subframe,
    ) -> Option<u64> {
        let order = coefs.len();
        let shift = lpc::quantize(coefs, precision, &mut q[..order])?;
        if !lpc_residual(x, &q[..order], shift, &mut self.tmp_res) {
            return None;
        }
        let (rb, po, r2) = rice_search(
            &self.tmp_res,
            x.len(),
            order,
            self.params.max_porder,
            &mut self.sums,
            &mut self.tmp_params,
        );
        let bits = head + order as u64 * u64::from(bps + precision) + 4 + 5 + 6 + rb;
        if bits >= out.bits {
            return None;
        }
        out.bits = bits;
        out.kind = Kind::Lpc {
            order,
            precision,
            shift,
            coefs: *q,
        };
        out.porder = po;
        out.rice2 = r2;
        std::mem::swap(&mut out.params, &mut self.tmp_params);
        std::mem::swap(&mut out.residual, &mut self.tmp_res);
        Some(bits)
    }
}

/// Residual of fixed predictor `order` for samples `order..n`; false if one does not fit in
/// 32 bits.
fn fixed_residual(x: &[i32], order: usize, res: &mut Vec<i32>) -> bool {
    res.clear();
    let n = x.len();
    let s = |i: usize| i64::from(x[i]);
    let mut ok = true;
    let mut push = |v: i64| {
        ok &= i32::try_from(v).is_ok();
        res.push(v as i32);
    };
    match order {
        0 => (0..n).for_each(|i| push(s(i))),
        1 => (1..n).for_each(|i| push(s(i) - s(i - 1))),
        2 => (2..n).for_each(|i| push(s(i) - 2 * s(i - 1) + s(i - 2))),
        3 => (3..n).for_each(|i| push(s(i) - 3 * s(i - 1) + 3 * s(i - 2) - s(i - 3))),
        _ => {
            (4..n).for_each(|i| push(s(i) - 4 * s(i - 1) + 6 * s(i - 2) - 4 * s(i - 3) + s(i - 4)))
        }
    }
    ok
}

/// LPC residual (`q[j]` multiplies `x[i - 1 - j]`); false if one does not fit in 32 bits.
fn lpc_residual(x: &[i32], q: &[i32], shift: u32, res: &mut Vec<i32>) -> bool {
    let order = q.len();
    res.clear();
    // oldest-first coefficients so the history window is walked forwards
    let mut c = [0i64; MAX_ORDER];
    for (j, &v) in q.iter().enumerate() {
        c[order - 1 - j] = i64::from(v);
    }
    let c = &c[..order];
    let mut ok = true;
    for w in x.windows(order + 1) {
        let mut sum = 0i64;
        for (a, &b) in c.iter().zip(w) {
            sum += a * i64::from(b);
        }
        let r = i64::from(w[order]) - (sum >> shift);
        ok &= i32::try_from(r).is_ok();
        res.push(r as i32);
    }
    ok
}

/// Choose the partition order (0..=max) and Rice parameters for `res` (the residual of
/// samples `order..n`). Returns (bits including parameters, partition order, rice2) and leaves
/// the parameters in `params`.
fn rice_search(
    res: &[i32],
    n: usize,
    order: usize,
    max_porder: u32,
    sums: &mut Vec<u64>,
    params: &mut Vec<u32>,
) -> (u64, u32, bool) {
    let mut top = max_porder.min(15);
    while top > 0 && ((n >> top) << top != n || (n >> top) < order.max(1)) {
        top -= 1;
    }
    // zig-zag sums of the finest partitions
    let parts = 1usize << top;
    let psize = n >> top;
    sums.clear();
    sums.resize(parts, 0);
    let mut start = 0;
    for (p, s) in sums.iter_mut().enumerate() {
        let end = (p + 1) * psize - order;
        *s = res[start..end].iter().map(|&r| u64::from(zigzag(r))).sum();
        start = end;
    }
    let mut best = (u64::MAX, 0u32, false);
    let mut level = top;
    let mut cur_params: Vec<u32> = Vec::new();
    loop {
        let parts = 1usize << level;
        let psize = (n >> level) as u64;
        let mut bits = 0u64;
        let mut max_k = 0;
        cur_params.clear();
        for (p, &s) in sums[..parts].iter().enumerate() {
            let cnt = psize - if p == 0 { order as u64 } else { 0 };
            let (b, k) = rice_cost(s, cnt);
            bits += b;
            max_k = max_k.max(k);
            cur_params.push(k);
        }
        let rice2 = max_k > 14;
        bits += parts as u64 * if rice2 { 5 } else { 4 };
        if bits < best.0 {
            best = (bits, level, rice2);
            params.clear();
            params.extend_from_slice(&cur_params);
        }
        if level == 0 {
            break;
        }
        // merge neighbouring partitions
        for p in 0..parts / 2 {
            sums[p] = sums[2 * p] + sums[2 * p + 1];
        }
        level -= 1;
    }
    best
}

/// Write one subframe.
fn write_subframe(w: &mut BitWriter, sf: &Subframe, n: usize) {
    let (ty, order) = match sf.kind {
        Kind::Constant => (0, 0),
        Kind::Verbatim => (1, 0),
        Kind::Fixed(o) => (8 + o as u32, o),
        Kind::Lpc { order, .. } => (31 + order as u32, order),
    };
    w.write(ty, 7); // zero padding bit + type
    if sf.wasted > 0 {
        w.write(1, 1);
        w.write_unary_zeros(sf.wasted - 1);
    } else {
        w.write(0, 1);
    }
    match sf.kind {
        Kind::Constant => w.write_signed(sf.signal[0], sf.bps),
        Kind::Verbatim => {
            for &s in &sf.signal[..n] {
                w.write_signed(s, sf.bps);
            }
        }
        Kind::Fixed(_) | Kind::Lpc { .. } => {
            for &s in &sf.signal[..order] {
                w.write_signed(s, sf.bps);
            }
            if let Kind::Lpc {
                precision,
                shift,
                coefs,
                ..
            } = sf.kind
            {
                w.write(precision - 1, 4);
                w.write(shift, 5);
                for &c in &coefs[..order] {
                    w.write_signed(c, precision);
                }
            }
            write_residual(w, sf, n, order);
        }
    }
}

fn write_residual(w: &mut BitWriter, sf: &Subframe, n: usize, order: usize) {
    let pbits = if sf.rice2 { 5 } else { 4 };
    w.write(u32::from(sf.rice2), 2);
    w.write(sf.porder, 4);
    let psize = n >> sf.porder;
    let mut start = 0;
    for (p, &k) in sf.params.iter().enumerate() {
        let end = (p + 1) * psize - order;
        w.write(k, pbits);
        let mask = (1u32 << k) - 1;
        for &r in &sf.residual[start..end] {
            let u = zigzag(r);
            let q = u >> k;
            if q + 1 + k <= 32 {
                w.write((1 << k) | (u & mask), q + 1 + k);
            } else {
                w.write_unary_zeros(q);
                w.write(u & mask, k);
            }
        }
        start = end;
    }
}

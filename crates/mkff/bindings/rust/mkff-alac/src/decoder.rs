//! ALAC packet decoder.

use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{AudioBuffer, Decoder, Error, Result};

use crate::config::{AlacConfig, CHANNEL_MAP};
use crate::predictor::unpredict;

/// Element tags.
pub(crate) const ID_SCE: u32 = 0;
pub(crate) const ID_CPE: u32 = 1;
pub(crate) const ID_CCE: u32 = 2;
pub(crate) const ID_LFE: u32 = 3;
pub(crate) const ID_DSE: u32 = 4;
pub(crate) const ID_PCE: u32 = 5;
pub(crate) const ID_FIL: u32 = 6;
pub(crate) const ID_END: u32 = 7;

/// Adaptive Golomb coder constants.
pub(crate) const QBSHIFT: u32 = 9;
pub(crate) const QB: u32 = 1 << QBSHIFT;
pub(crate) const MMULSHIFT: u32 = 2;
pub(crate) const MDENSHIFT: u32 = QBSHIFT - MMULSHIFT - 1;
pub(crate) const MOFF: u32 = 1 << (MDENSHIFT - 2);
pub(crate) const BITOFF: i32 = 24;
/// Unary prefixes this long (or longer) escape to a raw value.
pub(crate) const MAX_PREFIX: u32 = 9;
pub(crate) const MAX_RUN_BITS: u32 = 16;
pub(crate) const N_MAX_MEAN_CLAMP: u32 = 0xFFFF;

/// Apple Lossless decoder: one packet is one ALAC frame (all channel elements of
/// `frame_length` samples, or fewer for a partial last frame).
///
/// Output channels are in WAV order (see [`AlacConfig`] for the layouts).
#[derive(Clone, Debug)]
pub struct AlacDecoder {
    cfg: AlacConfig,
    /// Predictor buffers of the current element's two channels.
    pred: [Vec<i32>; 2],
    /// Low bits shifted off 24 / 32-bit samples, per element channel.
    low: [Vec<u32>; 2],
    /// Output, WAV order.
    out: Vec<Vec<i32>>,
    frames: usize,
}

/// Per-channel predictor parameters of a compressed element.
#[derive(Clone, Copy)]
struct ChannelParams {
    mode: u32,
    den_shift: u32,
    pb_factor: u32,
    num: usize,
    coefs: [i16; 32],
}

impl AlacDecoder {
    /// A decoder for the stream described by `cookie` (any form [`AlacConfig::parse`] takes).
    pub fn new(cookie: &[u8]) -> Result<Self> {
        Self::with_config(AlacConfig::parse(cookie)?)
    }

    /// A decoder for an already parsed configuration.
    pub fn with_config(cfg: AlacConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            pred: [Vec::new(), Vec::new()],
            low: [Vec::new(), Vec::new()],
            out: vec![Vec::new(); usize::from(cfg.num_channels)],
            frames: 0,
        })
    }

    /// The stream configuration.
    pub fn config(&self) -> &AlacConfig {
        &self.cfg
    }

    /// Decode one packet to integer samples: one `Vec` per channel (WAV order) and the bit
    /// depth. Bit-exact.
    pub fn decode_i32(&mut self, packet: &[u8]) -> Result<(Vec<Vec<i32>>, u32)> {
        self.decode_packet(packet)?;
        let out = self.out.iter().map(|c| c[..self.frames].to_vec()).collect();
        Ok((out, u32::from(self.cfg.bit_depth)))
    }

    fn decode_packet(&mut self, packet: &[u8]) -> Result<()> {
        let nch = usize::from(self.cfg.num_channels);
        let map = CHANNEL_MAP[nch - 1];
        let mut r = BitReader::new(packet);
        let mut ch = 0;
        let mut frames: Option<usize> = None;
        while ch < nch {
            match r.read(3)? {
                tag @ (ID_SCE | ID_LFE | ID_CPE) => {
                    let width = if tag == ID_CPE { 2 } else { 1 };
                    if ch + width > nch {
                        return Err(Error::invalid(
                            "ALAC packet has more channels than the config",
                        ));
                    }
                    let n = self.decode_element(&mut r, width)?;
                    if *frames.get_or_insert(n) != n {
                        return Err(Error::invalid("ALAC elements differ in length"));
                    }
                    for c in 0..width {
                        let dst = &mut self.out[map[ch + c]];
                        dst.clear();
                        dst.extend_from_slice(&self.pred[c][..n]);
                    }
                    ch += width;
                }
                ID_DSE => {
                    r.skip(4)?; // element instance tag
                    let align = r.read_bit()?;
                    let mut count = r.read(8)? as usize;
                    if count == 255 {
                        count += r.read(8)? as usize;
                    }
                    if align {
                        r.byte_align();
                    }
                    r.skip(count * 8)?;
                }
                ID_FIL => {
                    let mut count = r.read(4)? as usize;
                    if count == 15 {
                        count += r.read(8)? as usize;
                        count -= 1;
                    }
                    r.skip(count * 8)?;
                }
                ID_END => break,
                tag @ (ID_CCE | ID_PCE) => {
                    return Err(Error::unsupported(format!("ALAC element type {tag}")));
                }
                _ => unreachable!("3-bit tag"),
            }
        }
        if ch < nch {
            return Err(Error::invalid("ALAC packet ends before all channels"));
        }
        self.frames = frames.unwrap_or(0);
        Ok(())
    }

    /// Decode one SCE / LFE (`width` 1) or CPE (`width` 2) into `pred[..width]`; returns the
    /// sample count.
    fn decode_element(&mut self, r: &mut BitReader, width: usize) -> Result<usize> {
        r.skip(4)?; // element instance tag
        if r.read(12)? != 0 {
            return Err(Error::invalid("ALAC element header: unused bits set"));
        }
        let partial = r.read_bit()?;
        let bytes_shifted = r.read(2)?;
        let escape = r.read_bit()?;
        if bytes_shifted == 3 {
            return Err(Error::invalid("ALAC element shifts 3 bytes"));
        }
        let n = if partial {
            r.read(32)? as usize
        } else {
            self.cfg.frame_length as usize
        };
        if n > self.cfg.frame_length as usize {
            return Err(Error::invalid("ALAC element longer than the frame length"));
        }
        let depth = u32::from(self.cfg.bit_depth);
        for c in 0..width {
            self.pred[c].clear();
            self.pred[c].resize(n, 0);
        }

        if escape {
            // uncompressed: interleaved samples of the full bit depth
            for i in 0..n {
                for c in 0..width {
                    self.pred[c][i] = r.read_signed(depth)?;
                }
            }
            return Ok(n);
        }

        let shift = bytes_shifted * 8;
        let chan_bits = depth - shift + (width as u32 - 1);
        if chan_bits > 32 {
            return Err(Error::unsupported(
                "ALAC 32-bit channel pair without shifted bytes",
            ));
        }
        let mix_bits = r.read(8)?;
        let mix_res = r.read_signed(8)?;
        let mut params = [ChannelParams {
            mode: 0,
            den_shift: 0,
            pb_factor: 0,
            num: 0,
            coefs: [0; 32],
        }; 2];
        for p in params.iter_mut().take(width) {
            p.mode = r.read(4)?;
            p.den_shift = r.read(4)?;
            p.pb_factor = r.read(3)?;
            p.num = r.read(5)? as usize;
            for c in &mut p.coefs[..p.num] {
                *c = r.read_signed(16)? as i16;
            }
        }
        // the shifted-off low bits come first in the stream; read them after the residuals
        let low_pos = r.position();
        if shift > 0 {
            r.skip(shift as usize * width * n)?;
        }
        for (c, p) in params.iter_mut().enumerate().take(width) {
            let pb = u32::from(self.cfg.pb) * p.pb_factor / 4;
            let buf = &mut self.pred[c];
            decode_residuals(
                r,
                buf,
                pb,
                u32::from(self.cfg.mb),
                u32::from(self.cfg.kb),
                chan_bits,
            )?;
            if p.mode != 0 {
                // mode 15 (and Apple's decoder treats any non-zero mode alike): a first-order
                // pass runs before the adaptive filter
                unpredict(buf, &mut [], 31, chan_bits, 0);
            }
            let num = p.num;
            unpredict(buf, &mut p.coefs[..num], num, chan_bits, p.den_shift);
        }

        if width == 2 && mix_res != 0 {
            let (u, v) = self.pred.split_at_mut(1);
            let mb = mix_bits.min(63);
            for (a, b) in u[0].iter_mut().zip(v[0].iter_mut()) {
                let l =
                    i64::from(*a) + i64::from(*b) - ((i64::from(mix_res) * i64::from(*b)) >> mb);
                *a = l as i32;
                *b = (l - i64::from(*b)) as i32;
            }
        }
        if shift > 0 {
            let end = r.position();
            r.seek(low_pos);
            for c in 0..width {
                self.low[c].clear();
                self.low[c].resize(n, 0);
            }
            for i in 0..n {
                for c in 0..width {
                    self.low[c][i] = r.read(shift)?;
                }
            }
            r.seek(end);
            for c in 0..width {
                for (s, &lo) in self.pred[c].iter_mut().zip(&self.low[c]) {
                    *s = (s.wrapping_shl(shift)) | lo as i32;
                }
            }
        }
        Ok(n)
    }
}

impl Decoder for AlacDecoder {
    /// Decode one packet to planar f32: samples are divided by `2^(bit_depth - 1)`.
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        self.decode_packet(packet)?;
        let scale = 1.0 / (1u64 << (self.cfg.bit_depth - 1)) as f64;
        let channels = self
            .out
            .iter()
            .map(|c| {
                c[..self.frames]
                    .iter()
                    .map(|&s| (f64::from(s) * scale) as f32)
                    .collect()
            })
            .collect();
        Ok(AudioBuffer {
            sample_rate: self.cfg.sample_rate,
            channels,
        })
    }

    /// ALAC packets are independent: nothing to forget.
    fn reset(&mut self) {}

    fn sample_rate(&self) -> u32 {
        self.cfg.sample_rate
    }

    fn channels(&self) -> usize {
        usize::from(self.cfg.num_channels)
    }
}

/// Read one adaptive-Golomb value with parameter `k` (`m = 2^k - 1`): a unary prefix of ones
/// (at most [`MAX_PREFIX`]), then `k - 1` or `k` bits; a full prefix escapes to `escape_bits`
/// raw bits.
#[inline]
fn read_golomb(r: &mut BitReader, k: u32, m: u32, escape_bits: u32) -> Result<u32> {
    let p = r.peek(32);
    let pre = (!p).leading_zeros();
    if pre >= MAX_PREFIX {
        r.skip(MAX_PREFIX as usize)?;
        return r.read(escape_bits);
    }
    if k <= 1 {
        r.skip(pre as usize + 1)?;
        return Ok(if k == 1 { pre } else { 0 });
    }
    let v = if pre + 1 + k <= 32 {
        (p << (pre + 1)) >> (32 - k)
    } else {
        let mut t = r.clone();
        t.skip(pre as usize + 1)?;
        t.peek(k)
    };
    let base = pre.wrapping_mul(m);
    if v > 1 {
        r.skip((pre + 1 + k) as usize)?;
        Ok(base.wrapping_add(v - 1))
    } else {
        r.skip((pre + k) as usize)?;
        Ok(base)
    }
}

/// Adaptive Golomb-Rice decoding of one channel's prediction residuals (with zero runs).
pub(crate) fn decode_residuals(
    r: &mut BitReader,
    out: &mut [i32],
    pb: u32,
    mb0: u32,
    kb: u32,
    max_bits: u32,
) -> Result<()> {
    let n = out.len();
    let wb = if kb >= 32 { u32::MAX } else { (1u32 << kb) - 1 };
    let mut mb = mb0;
    let mut zmode = 0u32;
    let mut c = 0;
    while c < n {
        let k = (31 - ((mb >> QBSHIFT) + 3).leading_zeros()).min(kb);
        let v = read_golomb(r, k, (1u32 << k) - 1, max_bits)?;
        let nd = u64::from(v) + u64::from(zmode);
        let mag = ((nd + 1) >> 1) as u32 as i32;
        out[c] = if nd & 1 != 0 { mag.wrapping_neg() } else { mag };
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
            let run = read_golomb(r, k, ((1u32 << k) - 1) & wb, MAX_RUN_BITS)? as usize;
            if run > n - c {
                return Err(Error::invalid("ALAC zero run past the end of the frame"));
            }
            out[c..c + run].fill(0);
            c += run;
            if run >= 65535 {
                zmode = 0;
            }
            mb = 0;
        }
    }
    Ok(())
}

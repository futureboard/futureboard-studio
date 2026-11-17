//! FLAC frame decoder (RFC 9639 sections 9.2 - 9.3).

use mkff_audio_core::bits::BitReader;
use mkff_audio_core::crc::crc16;
use mkff_audio_core::{AudioBuffer, Decoder, Error, Result};

use crate::frame::{parse_frame_header, ChannelAssignment, FrameHeader};
use crate::StreamInfo;

/// FLAC decoder: one packet is one complete FLAC frame (header through CRC-16).
///
/// Every frame is checked: the header CRC-8 and the frame CRC-16 must match, and anything
/// malformed returns an error rather than partial output. Bytes after the frame's CRC-16 are
/// ignored. Frames are independent, so decoding can start at any frame.
///
/// ```no_run
/// use mkff_audio_core::Decoder;
/// # fn frames() -> Vec<Vec<u8>> { Vec::new() }
/// # fn main() -> mkff_audio_core::Result<()> {
/// # let streaminfo = mkff_flac::StreamInfo { bits_per_sample: 16, ..Default::default() }.to_bytes();
/// // `streaminfo`: from the container (MP4 `dfLa`, Matroska CodecPrivate) or the `fLaC` header
/// let mut dec = mkff_flac::FlacDecoder::new(&streaminfo)?;
/// for frame in frames() {
///     let pcm = dec.decode(&frame)?; // planar f32
///     # let _ = pcm;
/// }
/// # Ok(()) }
/// ```
#[derive(Clone, Debug)]
pub struct FlacDecoder {
    info: Option<StreamInfo>,
    sample_rate: u32,
    channels: usize,
    bits_per_sample: u32,
    /// Per-channel sample buffers of the last frame (i64: side channels of 32-bit audio are
    /// 33 bits wide).
    buf: Vec<Vec<i64>>,
    /// Block size of the last frame.
    frames: usize,
    /// Check the frame CRC-16 (only switched off by the internal fuzz test, to reach the
    /// subframe parser with damaged data).
    verify_crc: bool,
}

impl FlacDecoder {
    /// A decoder for a stream with this `STREAMINFO` (any form [`StreamInfo::parse`] accepts).
    ///
    /// `streaminfo` may be empty: the stream parameters then come from the frame headers, and
    /// frames whose header says "see STREAMINFO" for the bit depth fail to decode.
    pub fn new(streaminfo: &[u8]) -> Result<Self> {
        let info = if streaminfo.is_empty() {
            None
        } else {
            Some(StreamInfo::parse(streaminfo)?)
        };
        Ok(Self {
            sample_rate: info.map_or(0, |i| i.sample_rate),
            channels: info.map_or(0, |i| usize::from(i.channels)),
            bits_per_sample: info.map_or(0, |i| u32::from(i.bits_per_sample)),
            info,
            buf: Vec::new(),
            frames: 0,
            verify_crc: true,
        })
    }

    /// The `STREAMINFO` the decoder was created with.
    pub fn stream_info(&self) -> Option<&StreamInfo> {
        self.info.as_ref()
    }

    /// Bits per sample of the last decoded frame (or STREAMINFO's before the first frame).
    pub fn bits_per_sample(&self) -> u32 {
        self.bits_per_sample
    }

    /// Decode one frame to integer samples: one `Vec` per channel (WAV channel order) and the
    /// bits per sample. Bit-exact.
    pub fn decode_i32(&mut self, packet: &[u8]) -> Result<(Vec<Vec<i32>>, u32)> {
        self.decode_frame(packet)?;
        let out = self.buf[..self.channels]
            .iter()
            .map(|c| c[..self.frames].iter().map(|&s| s as i32).collect())
            .collect();
        Ok((out, self.bits_per_sample))
    }

    /// Decode one frame into the internal buffers; returns the frame header.
    fn decode_frame(&mut self, packet: &[u8]) -> Result<FrameHeader> {
        let hdr = parse_frame_header(packet).ok_or_else(|| {
            Error::invalid("bad FLAC frame header (sync, reserved code or CRC-8)")
        })?;
        let bps = match hdr.bits_per_sample {
            Some(b) => b,
            None => match self.info {
                Some(i) => u32::from(i.bits_per_sample),
                None => {
                    return Err(Error::invalid(
                        "frame takes bits per sample from STREAMINFO",
                    ))
                }
            },
        };
        if !(4..=32).contains(&bps) {
            return Err(Error::unsupported(format!("FLAC {bps} bits per sample")));
        }
        let sample_rate = hdr
            .sample_rate
            .or(self.info.map(|i| i.sample_rate))
            .unwrap_or(0);
        let n = hdr.block_size as usize;
        let nch = hdr.channels();
        if self.buf.len() < nch {
            self.buf.resize_with(nch, Vec::new);
        }

        let mut r = BitReader::new(packet);
        r.seek(hdr.header_len * 8);
        for ch in 0..nch {
            let sbps = bps + u32::from(hdr.channel_assignment.is_side(ch));
            let out = &mut self.buf[ch];
            out.clear();
            out.resize(n, 0);
            decode_subframe(&mut r, out, sbps)?;
        }
        r.byte_align();
        let end = r.byte_pos();
        let crc = packet.get(end..end + 2).ok_or(Error::EndOfData)?;
        if self.verify_crc && crc16(0, &packet[..end]) != u16::from_be_bytes([crc[0], crc[1]]) {
            return Err(Error::invalid("FLAC frame CRC-16 mismatch"));
        }

        if nch == 2 {
            let (a, b) = self.buf.split_at_mut(1);
            let (a, b) = (&mut a[0][..n], &mut b[0][..n]);
            match hdr.channel_assignment {
                ChannelAssignment::LeftSide => {
                    for (l, s) in a.iter().zip(b.iter_mut()) {
                        *s = l.wrapping_sub(*s);
                    }
                }
                ChannelAssignment::SideRight => {
                    for (s, r) in a.iter_mut().zip(b.iter()) {
                        *s = s.wrapping_add(*r);
                    }
                }
                ChannelAssignment::MidSide => {
                    for (m, s) in a.iter_mut().zip(b.iter_mut()) {
                        let mid = (m.wrapping_shl(1)) | (*s & 1);
                        let side = *s;
                        *m = mid.wrapping_add(side) >> 1;
                        *s = mid.wrapping_sub(side) >> 1;
                    }
                }
                ChannelAssignment::Independent(_) => {}
            }
        }
        self.frames = n;
        self.channels = nch;
        self.bits_per_sample = bps;
        self.sample_rate = sample_rate;
        Ok(hdr)
    }
}

impl Decoder for FlacDecoder {
    /// Decode one frame to planar f32: samples are divided by `2^(bps - 1)`.
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        self.decode_frame(packet)?;
        let scale = 1.0 / (1u64 << (self.bits_per_sample - 1)) as f64;
        let channels = self.buf[..self.channels]
            .iter()
            .map(|c| {
                c[..self.frames]
                    .iter()
                    .map(|&s| (s as f64 * scale) as f32)
                    .collect()
            })
            .collect();
        Ok(AudioBuffer {
            sample_rate: self.sample_rate,
            channels,
        })
    }

    /// FLAC frames are independent: nothing to forget.
    fn reset(&mut self) {}

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> usize {
        self.channels
    }
}

/// Read an `n`-bit (1..=33) two's-complement value.
#[inline]
fn read_wide(r: &mut BitReader, n: u32) -> Result<i64> {
    if n <= 32 {
        Ok(i64::from(r.read_signed(n)?))
    } else {
        let v = r.read_u64(n)?;
        Ok(((v << (64 - n)) as i64) >> (64 - n))
    }
}

/// Decode one subframe of `bps` bits per sample (including a side channel's extra bit) into
/// `out` (pre-sized to the block size).
fn decode_subframe(r: &mut BitReader, out: &mut [i64], bps: u32) -> Result<()> {
    if r.read_bit()? {
        return Err(Error::invalid("FLAC subframe padding bit set"));
    }
    let kind = r.read(6)?;
    let wasted = if r.read_bit()? {
        r.read_unary_zeros()? + 1
    } else {
        0
    };
    if wasted >= bps {
        return Err(Error::invalid("FLAC wasted bits exceed sample size"));
    }
    let bps = bps - wasted;
    let n = out.len();
    match kind {
        0 => {
            let v = read_wide(r, bps)?;
            out.fill(v);
        }
        1 => {
            for s in out.iter_mut() {
                *s = read_wide(r, bps)?;
            }
        }
        8..=12 => {
            let order = (kind - 8) as usize;
            if order > n {
                return Err(Error::invalid("FLAC predictor order exceeds block size"));
            }
            for s in &mut out[..order] {
                *s = read_wide(r, bps)?;
            }
            decode_residual(r, out, order)?;
            restore_fixed(out, order);
        }
        32..=63 => {
            let order = (kind - 31) as usize;
            if order > n {
                return Err(Error::invalid("FLAC predictor order exceeds block size"));
            }
            for s in &mut out[..order] {
                *s = read_wide(r, bps)?;
            }
            let precision = r.read(4)? + 1;
            if precision == 16 {
                return Err(Error::invalid("FLAC LPC precision code 15"));
            }
            let shift = r.read_signed(5)?;
            if shift < 0 {
                return Err(Error::invalid("FLAC negative LPC shift"));
            }
            let mut coefs = [0i64; 32];
            // stored newest-first; keep oldest-first to walk the history forwards
            for j in 0..order {
                coefs[order - 1 - j] = i64::from(r.read_signed(precision)?);
            }
            decode_residual(r, out, order)?;
            restore_lpc(out, &coefs[..order], shift as u32);
        }
        _ => {
            return Err(Error::invalid(format!(
                "reserved FLAC subframe type {kind}"
            )))
        }
    }
    if wasted > 0 {
        for s in out.iter_mut() {
            *s = s.wrapping_shl(wasted);
        }
    }
    Ok(())
}

/// Read the partitioned Rice residual into `out[order..]`.
fn decode_residual(r: &mut BitReader, out: &mut [i64], order: usize) -> Result<()> {
    let (param_bits, escape) = match r.read(2)? {
        0 => (4, 15),
        1 => (5, 31),
        _ => return Err(Error::invalid("reserved FLAC residual coding method")),
    };
    let porder = r.read(4)?;
    let n = out.len();
    let psize = n >> porder;
    if psize << porder != n || psize < order {
        return Err(Error::invalid(
            "FLAC partition order does not fit the block",
        ));
    }
    let mut start = order;
    for p in 0..1usize << porder {
        let end = (p + 1) * psize;
        let k = r.read(param_bits)?;
        let part = &mut out[start..end];
        if k == escape {
            let bits = r.read(5)?;
            for s in part.iter_mut() {
                *s = i64::from(r.read_signed(bits)?);
            }
        } else {
            read_rice_partition(r, part, k)?;
        }
        start = end;
    }
    Ok(())
}

/// Rice-decode `part.len()` values with parameter `k`.
#[inline]
fn read_rice_partition(r: &mut BitReader, part: &mut [i64], k: u32) -> Result<()> {
    let total = r.len_bits();
    for s in part.iter_mut() {
        // fast path: quotient and remainder within the next 32 bits
        let p = r.peek(32);
        let q = p.leading_zeros();
        let u: u64 = if q + 1 + k <= 32 {
            let len = q + 1 + k;
            if r.position() + len as usize > total {
                return Err(Error::EndOfData);
            }
            r.seek(r.position() + len as usize);
            u64::from(if k == 0 { 0 } else { p << (q + 1) >> (32 - k) }) | (u64::from(q) << k)
        } else {
            let q = r.read_unary_zeros()?;
            (u64::from(q) << k) | u64::from(r.read(k)?)
        };
        *s = ((u >> 1) as i64) ^ -((u & 1) as i64);
    }
    Ok(())
}

/// Undo a fixed predictor in place (`out[..order]` are warm-up samples, the rest residuals).
fn restore_fixed(out: &mut [i64], order: usize) {
    let n = out.len();
    match order {
        0 => {}
        1 => {
            for i in 1..n {
                out[i] = out[i].wrapping_add(out[i - 1]);
            }
        }
        2 => {
            for i in 2..n {
                let p = out[i - 1].wrapping_mul(2).wrapping_sub(out[i - 2]);
                out[i] = out[i].wrapping_add(p);
            }
        }
        3 => {
            for i in 3..n {
                let p = out[i - 1]
                    .wrapping_sub(out[i - 2])
                    .wrapping_mul(3)
                    .wrapping_add(out[i - 3]);
                out[i] = out[i].wrapping_add(p);
            }
        }
        _ => {
            for i in 4..n {
                let p = out[i - 1]
                    .wrapping_add(out[i - 3])
                    .wrapping_mul(4)
                    .wrapping_sub(out[i - 2].wrapping_mul(6))
                    .wrapping_sub(out[i - 4]);
                out[i] = out[i].wrapping_add(p);
            }
        }
    }
}

/// Undo an LPC predictor in place. `coefs` are oldest-first: `coefs[j]` multiplies
/// `out[i - order + j]`.
fn restore_lpc(out: &mut [i64], coefs: &[i64], shift: u32) {
    let order = coefs.len();
    for i in order..out.len() {
        let hist = &out[i - order..i];
        let mut sum = 0i64;
        for (c, s) in coefs.iter().zip(hist) {
            sum = sum.wrapping_add(c.wrapping_mul(*s));
        }
        out[i] = out[i].wrapping_add(sum >> shift);
    }
}

#[cfg(test)]
// subframe headers are written as padding bit _ type _ wasted flag
#[allow(clippy::unusual_byte_groupings)]
mod tests {
    use super::*;
    use crate::frame::write_coded_number;
    use mkff_audio_core::bits::BitWriter;
    use mkff_audio_core::crc::crc8;

    /// A complete frame: header for `n` samples (8-bit block size field), 44.1 kHz,
    /// `ch_code` / `bps_code`, then `body` writes the subframes.
    fn frame(
        n: usize,
        ch_code: u8,
        bps_code: u8,
        variable: Option<u64>,
        body: impl FnOnce(&mut BitWriter),
    ) -> Vec<u8> {
        assert!((1..=256).contains(&n));
        let mut h = vec![
            0xFF,
            0xF8 | u8::from(variable.is_some()),
            0x69,
            (ch_code << 4) | (bps_code << 1),
        ];
        let mut num = [0u8; 7];
        let len = write_coded_number(variable.unwrap_or(0), &mut num);
        h.extend_from_slice(&num[..len]);
        h.push((n - 1) as u8);
        h.push(crc8(&h));
        let mut w = BitWriter::new();
        w.write_bytes(&h);
        body(&mut w);
        w.byte_align();
        let crc = crc16(0, w.bytes());
        w.write(u32::from(crc), 16);
        w.finish()
    }

    fn rice(w: &mut BitWriter, v: i32, k: u32) {
        let u = ((v << 1) ^ (v >> 31)) as u32;
        w.write_unary_zeros(u >> k);
        w.write(u & ((1 << k) - 1), k);
    }

    fn decode(f: &[u8]) -> Result<(Vec<Vec<i32>>, u32)> {
        FlacDecoder::new(&[])?.decode_i32(f)
    }

    #[test]
    fn verbatim_constant_and_escape_partitions() {
        let vals: Vec<i32> = (0..16).map(|i| i * 37 - 300).collect();
        let f = frame(16, 1, 4, None, |w| {
            // channel 0: verbatim
            w.write(0b0_000001_0, 8);
            for &v in &vals {
                w.write_signed(v, 16);
            }
            // channel 1: fixed order 0, Rice, 2 partitions: escape (10 raw bits), then k = 3
            w.write(0b0_001000_0, 8);
            w.write(0, 2);
            w.write(1, 4);
            w.write(15, 4);
            w.write(10, 5);
            for &v in &vals[..8] {
                w.write_signed(v, 10);
            }
            w.write(3, 4);
            for &v in &vals[8..] {
                rice(w, v, 3);
            }
        });
        let (pcm, bps) = decode(&f).unwrap();
        assert_eq!(bps, 16);
        assert_eq!(pcm, vec![vals.clone(), vals.clone()]);
        // escape with zero bits (all residuals 0) after a fixed order 1; constant subframe;
        // variable block size header
        let f = frame(16, 1, 4, Some(4096), |w| {
            w.write(0, 8);
            w.write_signed(-5, 16);
            w.write(0b0_001001_0, 8);
            w.write_signed(7, 16);
            w.write(0, 2);
            w.write(0, 4);
            w.write(15, 4);
            w.write(0, 5);
        });
        let (pcm, _) = decode(&f).unwrap();
        assert_eq!(pcm, vec![vec![-5; 16], vec![7; 16]]);
    }

    #[test]
    fn rice2_and_lpc_order32() {
        // LPC order 32, precision 15, shift 13: only coef[0] = 1.0, i.e. x[n] = x[n-1] + r
        let n = 64;
        let res: Vec<i32> = (0..n as i32).map(|i| (i - 30) * 70_001).collect();
        let f = frame(n, 0, 6, None, |w| {
            w.write(0b0_111111_0, 8);
            for i in 0..32 {
                w.write_signed(i, 24);
            }
            w.write(14, 4); // precision 15
            w.write(13, 5); // shift
            w.write_signed(1 << 13, 15);
            for _ in 1..32 {
                w.write_signed(0, 15);
            }
            w.write(1, 2); // Rice2
            w.write(0, 4);
            w.write(20, 5);
            for &r in &res[32..] {
                rice(w, r, 20);
            }
        });
        let (pcm, bps) = decode(&f).unwrap();
        assert_eq!(bps, 24);
        let mut expect: Vec<i32> = (0..32).collect();
        for &r in &res[32..] {
            let last = *expect.last().unwrap();
            expect.push(last + r);
        }
        assert_eq!(pcm[0], expect);
    }

    #[test]
    fn side_channel_33_bits_verbatim() {
        // left/side, 32-bit: left = i32::MAX, right = i32::MIN, side = 2^32 - 1 (33 bits)
        let f = frame(4, 8, 7, None, |w| {
            w.write(0b0_000001_0, 8);
            for _ in 0..4 {
                w.write_signed(i32::MAX, 32);
            }
            w.write(0b0_000001_0, 8);
            for _ in 0..4 {
                w.write_u64((1u64 << 32) - 1, 33);
            }
        });
        let (pcm, bps) = decode(&f).unwrap();
        assert_eq!(bps, 32);
        assert_eq!(pcm, vec![vec![i32::MAX; 4], vec![i32::MIN; 4]]);
        // mid/side with wasted bits on the mid channel
        let f = frame(2, 10, 4, None, |w| {
            w.write(0b0_000001_1, 8);
            w.write(1, 1); // wasted = 1
            w.write_signed(100 >> 1, 15);
            w.write_signed(-4 >> 1, 15);
            w.write(0b0_000001_0, 8);
            w.write_signed(5, 17);
            w.write_signed(-1, 17);
        });
        // mid 100, side 5: L = ((100 << 1 | 1) + 5) >> 1 = 103, R = 98
        // mid -4, side -1: L = ((-8 | 1) - 1) >> 1 = -4, R = (-7 + 1) >> 1 = -3
        let (pcm, _) = decode(&f).unwrap();
        assert_eq!(pcm, vec![vec![103, -4], vec![98, -3]]);
        // side/right
        let f = frame(1, 9, 4, None, |w| {
            w.write(0b0_000001_0, 8);
            w.write_signed(-20, 17);
            w.write(0b0_000001_0, 8);
            w.write_signed(30, 16);
        });
        assert_eq!(decode(&f).unwrap().0, vec![vec![10], vec![30]]);
    }

    #[test]
    fn invalid_subframes_are_errors() {
        type Body = Box<dyn Fn(&mut BitWriter)>;
        let cases: Vec<(&str, Body)> = vec![
            (
                "negative shift",
                Box::new(|w| {
                    w.write(0b0_100000_0, 8);
                    w.write_signed(0, 16);
                    w.write(11, 4);
                    w.write_signed(-1, 5);
                }),
            ),
            (
                "precision 16",
                Box::new(|w| {
                    w.write(0b0_100000_0, 8);
                    w.write_signed(0, 16);
                    w.write(15, 4);
                }),
            ),
            ("reserved type", Box::new(|w| w.write(0b0_000010_0, 8))),
            ("fixed order 5", Box::new(|w| w.write(0b0_001101_0, 8))),
            ("padding bit", Box::new(|w| w.write(0b1_000000_0, 8))),
            (
                "residual method 2",
                Box::new(|w| {
                    w.write(0b0_001000_0, 8);
                    w.write(2, 2);
                }),
            ),
            (
                "partition order",
                Box::new(|w| {
                    w.write(0b0_001000_0, 8);
                    w.write(0, 2);
                    w.write(5, 4); // 16 >> 5 does not divide
                }),
            ),
            (
                "wasted >= bps",
                Box::new(|w| {
                    w.write(0b0_000000_1, 8);
                    w.write_unary_zeros(16);
                }),
            ),
        ];
        for (name, body) in cases {
            let f = frame(16, 0, 4, None, |w| {
                body(w);
                w.write(0, 32);
            });
            assert!(decode(&f).is_err(), "{name}");
        }
        // bits per sample from STREAMINFO without STREAMINFO
        let f = frame(16, 0, 0, None, |w| {
            w.write(0, 8);
            w.write(0, 16);
        });
        assert!(decode(&f).is_err());
        let si = StreamInfo {
            bits_per_sample: 16,
            channels: 1,
            sample_rate: 44100,
            ..Default::default()
        };
        assert!(FlacDecoder::new(&si.to_bytes())
            .unwrap()
            .decode_i32(&f)
            .is_ok());
    }

    #[test]
    fn fuzz_subframe_parser_without_crc() {
        use crate::{FlacEncoder, FlacEncoderConfig};
        let cfg = FlacEncoderConfig {
            channels: 2,
            block_size: 1152,
            ..Default::default()
        };
        let mut enc = FlacEncoder::new(cfg).unwrap();
        let a: Vec<i32> = (0..1152)
            .map(|i| ((i as f32 * 0.03).sin() * 9000.0) as i32 + (i * 7919 % 61))
            .collect();
        let b: Vec<i32> = a.iter().map(|s| s / 2 + 3).collect();
        let pkt = enc.encode_i32(&[&a, &b]).unwrap().remove(0);
        let mut dec = FlacDecoder::new(&[]).unwrap();
        dec.verify_crc = false;
        assert_eq!(dec.decode_i32(&pkt.data).unwrap().0, vec![a, b]);
        let mut seed = 7u32;
        for _ in 0..20_000 {
            let mut g = pkt.data.clone();
            for _ in 0..1 + seed % 4 {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                // keep the frame header intact so the subframes get parsed
                let pos = 6 + (seed >> 8) as usize % (g.len() - 6);
                g[pos] ^= 1 << (seed % 8);
            }
            let cut = g.len() - (seed >> 20) as usize % 64;
            let _ = dec.decode(&g[..cut.max(6)]);
        }
    }
}

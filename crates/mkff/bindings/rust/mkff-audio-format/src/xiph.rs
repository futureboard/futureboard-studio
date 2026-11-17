//! Vorbis and Opus header fields the containers need, and Xiph lacing.

use mkff_audio_core::{Error, Result};

use crate::io::{le16, le32};

/// Pack packets with Xiph lacing as in Matroska `CodecPrivate`: a byte holding `count - 1`, the
/// sizes of all packets but the last as runs of 255 plus a final byte < 255, then the packets.
pub fn xiph_lace(packets: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    if packets.is_empty() {
        return out;
    }
    out.push((packets.len() - 1) as u8);
    for p in &packets[..packets.len() - 1] {
        let mut n = p.len();
        while n >= 255 {
            out.push(255);
            n -= 255;
        }
        out.push(n as u8);
    }
    for p in packets {
        out.extend_from_slice(p);
    }
    out
}

/// Split Xiph-laced data (see [`xiph_lace`]) back into packets.
pub fn xiph_unlace(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let bad = || Error::invalid("bad Xiph lacing");
    let count = usize::from(*data.first().ok_or_else(bad)?) + 1;
    let mut p = 1;
    let mut sizes = Vec::with_capacity(count);
    for _ in 0..count - 1 {
        let mut n = 0usize;
        loop {
            let b = *data.get(p).ok_or_else(bad)?;
            p += 1;
            n += usize::from(b);
            if b < 255 {
                break;
            }
        }
        sizes.push(n);
    }
    let used: usize = sizes.iter().sum();
    if p + used > data.len() {
        return Err(bad());
    }
    sizes.push(data.len() - p - used);
    let mut out = Vec::with_capacity(count);
    for n in sizes {
        out.push(data[p..p + n].to_vec());
        p += n;
    }
    Ok(out)
}

/// Samples (at 48 kHz) an Opus packet decodes to, from its TOC byte and frame count (RFC 6716
/// §3.1). `None` for an empty or malformed packet.
pub fn opus_packet_duration(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config & 3)],
        12..=15 => [480, 960][usize::from(config & 1)],
        _ => [120, 240, 480, 960][usize::from(config & 3)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    let d = frame * frames;
    (d <= 5760).then_some(d)
}

/// The fields of an `OpusHead` packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpusHead {
    pub channels: u32,
    pub pre_skip: u32,
    pub input_rate: u32,
}

impl OpusHead {
    pub fn parse(p: &[u8]) -> Result<Self> {
        if p.len() < 19 || &p[..8] != b"OpusHead" || p[8] >> 4 != 0 {
            return Err(Error::invalid("bad OpusHead"));
        }
        let channels = u32::from(p[9]);
        if channels == 0 {
            return Err(Error::invalid("OpusHead with 0 channels"));
        }
        Ok(Self {
            channels,
            pre_skip: u32::from(le16(p, 10)),
            input_rate: le32(p, 12),
        })
    }
}

/// Build an `OpusHead` from a `dOps` box payload (Opus in ISOBMFF: big-endian fields).
pub(crate) fn opus_head_from_dops(d: &[u8]) -> Result<Vec<u8>> {
    if d.len() < 11 || d[0] != 0 {
        return Err(Error::invalid("bad dOps"));
    }
    let mut h = Vec::with_capacity(19 + d.len().saturating_sub(11));
    h.extend_from_slice(b"OpusHead");
    h.push(1);
    h.push(d[1]);
    h.extend_from_slice(&u16::from_be_bytes([d[2], d[3]]).to_le_bytes());
    h.extend_from_slice(&u32::from_be_bytes([d[4], d[5], d[6], d[7]]).to_le_bytes());
    h.extend_from_slice(&i16::from_be_bytes([d[8], d[9]]).to_le_bytes());
    h.push(d[10]);
    if d[10] != 0 {
        let n = 2 + usize::from(d[1]);
        let t = d
            .get(11..11 + n)
            .ok_or_else(|| Error::invalid("short dOps channel mapping"))?;
        h.extend_from_slice(t);
    }
    Ok(h)
}

/// Build a `dOps` payload from an `OpusHead` packet.
pub(crate) fn dops_from_opus_head(h: &[u8]) -> Result<Vec<u8>> {
    OpusHead::parse(h)?;
    let mut d = vec![0u8, h[9]];
    d.extend_from_slice(&le16(h, 10).to_be_bytes());
    d.extend_from_slice(&le32(h, 12).to_be_bytes());
    d.extend_from_slice(&(le16(h, 16)).to_be_bytes());
    d.push(h[18]);
    if h[18] != 0 {
        let n = 2 + usize::from(h[9]);
        d.extend_from_slice(
            h.get(19..19 + n)
                .ok_or_else(|| Error::invalid("short OpusHead channel mapping"))?,
        );
    }
    Ok(d)
}

/// What the Vorbis identification and setup headers tell a container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VorbisInfo {
    pub channels: u32,
    pub sample_rate: u32,
    pub blocksizes: [u32; 2],
    /// `blockflag` of each mode (from the setup header).
    pub mode_blockflags: Vec<bool>,
}

impl VorbisInfo {
    pub fn parse_id(p: &[u8]) -> Result<(u32, u32, [u32; 2])> {
        if p.len() < 30 || p[0] != 1 || &p[1..7] != b"vorbis" || le32(p, 7) != 0 {
            return Err(Error::invalid("bad Vorbis identification header"));
        }
        let channels = u32::from(p[11]);
        let rate = le32(p, 12);
        let b0 = p[28] & 0xF;
        let b1 = p[28] >> 4;
        if channels == 0
            || rate == 0
            || !(6..=13).contains(&b0)
            || !(6..=13).contains(&b1)
            || b0 > b1
        {
            return Err(Error::invalid("bad Vorbis identification header"));
        }
        Ok((channels, rate, [1 << b0, 1 << b1]))
    }

    /// Parse the mode list from the end of the setup header. The setup header ends with
    /// `vorbis_mode_count - 1` (6 bits), then per mode `blockflag` (1), `windowtype` (16, zero),
    /// `transformtype` (16, zero) and `mapping` (8), then the framing bit. Walking backwards
    /// from the framing bit, every 41-bit group whose window and transform types are zero is a
    /// candidate mode; the largest count whose preceding 6-bit field agrees is taken.
    pub fn parse_setup_modes(setup: &[u8]) -> Result<Vec<bool>> {
        if setup.len() < 7 || setup[0] != 5 || &setup[1..7] != b"vorbis" {
            return Err(Error::invalid("bad Vorbis setup header"));
        }
        let bit = |i: usize| (setup[i / 8] >> (i % 8)) & 1;
        let field =
            |start: usize, n: usize| (0..n).fold(0u32, |a, j| a | u32::from(bit(start + j)) << j);
        // the framing bit is the last set bit of the packet
        let last = setup
            .iter()
            .rposition(|&b| b != 0)
            .ok_or_else(|| Error::invalid("empty setup header"))?;
        let framing = last * 8 + (7 - setup[last].leading_zeros() as usize);
        let min_start = 7 * 8;
        let mut best = 0usize;
        for k in 1..=64usize {
            if framing < 41 * k + 6 || framing - 41 * k - 6 < min_start {
                break;
            }
            let s = framing - 41 * k;
            if field(s + 1, 16) != 0 || field(s + 17, 16) != 0 || field(s + 33, 8) > 63 {
                break;
            }
            if field(s - 6, 6) as usize + 1 == k {
                best = k;
            }
        }
        if best == 0 {
            return Err(Error::invalid("cannot find the Vorbis mode list"));
        }
        Ok((0..best)
            .map(|i| bit(framing - 41 * (best - i)) == 1)
            .collect())
    }

    /// Block size of an audio packet (`None` for header packets or bad modes).
    pub fn packet_blocksize(&self, p: &[u8]) -> Option<u32> {
        let b = *p.first()?;
        if b & 1 != 0 {
            return None;
        }
        let n = self.mode_blockflags.len();
        let bits = usize::BITS - (n.saturating_sub(1)).leading_zeros();
        let mode = (usize::from(b >> 1)) & ((1usize << bits) - 1);
        let long = *self.mode_blockflags.get(mode)?;
        Some(self.blocksizes[usize::from(long)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lacing_round_trip() {
        let a = vec![1u8; 300];
        let b = vec![2u8; 255];
        let c = vec![3u8; 7];
        let laced = xiph_lace(&[&a, &b, &c]);
        assert_eq!(&laced[..5], &[2, 255, 45, 255, 0]);
        let back = xiph_unlace(&laced).unwrap();
        assert_eq!(back, vec![a, b, c]);
        assert!(xiph_unlace(&[2, 255]).is_err());
    }

    #[test]
    fn opus_durations() {
        assert_eq!(opus_packet_duration(&[0xFC]), Some(960)); // CELT FB 20 ms
        assert_eq!(opus_packet_duration(&[0x08 | 1]), Some(1920)); // SILK NB 20 ms x2
        assert_eq!(opus_packet_duration(&[0xE3, 0x03]), Some(360));
        assert_eq!(opus_packet_duration(&[0x1B, 0x05]), None); // 5 x 60 ms > 120 ms
        assert_eq!(opus_packet_duration(&[]), None);
    }

    #[test]
    fn dops_round_trip() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let d = dops_from_opus_head(&head).unwrap();
        assert_eq!(d, vec![0, 2, 0x01, 0x38, 0, 0, 0xbb, 0x80, 0, 0, 0]);
        assert_eq!(opus_head_from_dops(&d).unwrap(), head);
    }
}

//! FLAC frame headers (RFC 9639 section 9.1).

use mkff_audio_core::crc::crc8;

/// How the channels of a frame are coded (RFC 9639 section 9.1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelAssignment {
    /// 1..=8 channels, each coded on its own.
    Independent(u8),
    /// Stereo: left, then side (left - right).
    LeftSide,
    /// Stereo: side (left - right), then right.
    SideRight,
    /// Stereo: mid ((left + right) >> 1), then side (left - right).
    MidSide,
}

impl ChannelAssignment {
    /// Channels in the frame.
    pub fn channels(self) -> usize {
        match self {
            ChannelAssignment::Independent(n) => n as usize,
            _ => 2,
        }
    }

    /// Whether subframe `ch` carries a side channel (one extra bit per sample).
    pub fn is_side(self, ch: usize) -> bool {
        matches!(
            (self, ch),
            (ChannelAssignment::LeftSide, 1)
                | (ChannelAssignment::SideRight, 0)
                | (ChannelAssignment::MidSide, 1)
        )
    }

    /// The 4-bit channel assignment code.
    pub fn code(self) -> u8 {
        match self {
            ChannelAssignment::Independent(n) => n.clamp(1, 8) - 1,
            ChannelAssignment::LeftSide => 8,
            ChannelAssignment::SideRight => 9,
            ChannelAssignment::MidSide => 10,
        }
    }
}

/// A parsed FLAC frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameHeader {
    /// Blocking strategy bit: `true` = variable block size (the coded number is a sample
    /// number), `false` = fixed (the coded number is a frame number).
    pub variable_block_size: bool,
    /// Samples per channel in the frame (1..=65535).
    pub block_size: u32,
    /// Sample rate in Hz, or `None` for "take it from STREAMINFO".
    pub sample_rate: Option<u32>,
    /// Channel coding.
    pub channel_assignment: ChannelAssignment,
    /// Bits per sample, or `None` for "take it from STREAMINFO".
    pub bits_per_sample: Option<u32>,
    /// Frame number (fixed block size) or number of the first sample (variable block size).
    pub number: u64,
    /// Header length in bytes, including the CRC-8.
    pub header_len: usize,
}

impl FrameHeader {
    /// Channels in the frame.
    pub fn channels(&self) -> usize {
        self.channel_assignment.channels()
    }
}

/// Standard sample rates of codes 1..=11.
const RATES: [u32; 11] = [
    88200, 176400, 192000, 8000, 16000, 22050, 24000, 32000, 44100, 48000, 96000,
];

/// Parse and CRC-8-check a frame header at the start of `data`.
///
/// Returns `None` if `data` does not start with a valid header: wrong sync code, a reserved or
/// forbidden code, a malformed coded number, a truncated header or a CRC-8 mismatch.
pub fn parse_frame_header(data: &[u8]) -> Option<FrameHeader> {
    if data.len() < 6 || data[0] != 0xFF || data[1] & 0xFE != 0xF8 {
        return None;
    }
    let variable = data[1] & 1 != 0;
    let bs_code = data[2] >> 4;
    let sr_code = data[2] & 0x0F;
    let ch_code = data[3] >> 4;
    let bps_code = (data[3] >> 1) & 7;
    if data[3] & 1 != 0 || bs_code == 0 || sr_code == 15 || ch_code > 10 || bps_code == 3 {
        return None;
    }
    let (number, n) = read_coded_number(&data[4..], if variable { 7 } else { 6 })?;
    let mut pos = 4 + n;
    let block_size = match bs_code {
        1 => 192,
        2..=5 => 576 << (bs_code - 2),
        6 => {
            let v = u32::from(*data.get(pos)?) + 1;
            pos += 1;
            v
        }
        7 => {
            let v = u32::from(u16::from_be_bytes([*data.get(pos)?, *data.get(pos + 1)?])) + 1;
            pos += 2;
            if v > 65535 {
                return None;
            }
            v
        }
        _ => 256 << (bs_code - 8),
    };
    let sample_rate = match sr_code {
        0 => None,
        1..=11 => Some(RATES[sr_code as usize - 1]),
        12 => {
            let v = u32::from(*data.get(pos)?) * 1000;
            pos += 1;
            Some(v)
        }
        _ => {
            let v = u32::from(u16::from_be_bytes([*data.get(pos)?, *data.get(pos + 1)?]));
            pos += 2;
            Some(if sr_code == 14 { v * 10 } else { v })
        }
    };
    let bits_per_sample = match bps_code {
        0 => None,
        1 => Some(8),
        2 => Some(12),
        4 => Some(16),
        5 => Some(20),
        6 => Some(24),
        _ => Some(32),
    };
    if crc8(&data[..pos]) != *data.get(pos)? {
        return None;
    }
    let channel_assignment = match ch_code {
        0..=7 => ChannelAssignment::Independent(ch_code + 1),
        8 => ChannelAssignment::LeftSide,
        9 => ChannelAssignment::SideRight,
        _ => ChannelAssignment::MidSide,
    };
    Some(FrameHeader {
        variable_block_size: variable,
        block_size,
        sample_rate,
        channel_assignment,
        bits_per_sample,
        number,
        header_len: pos + 1,
    })
}

/// Offset of the first valid frame header (sync code and CRC-8 checked) in `data`.
pub fn find_frame_sync(data: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < data.len() {
        // jump to the next 0xFF
        i += data[i..data.len() - 1].iter().position(|&b| b == 0xFF)?;
        if data[i + 1] & 0xFE == 0xF8 && parse_frame_header(&data[i..]).is_some() {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Decode the UTF-8-style coded number (at most `max_len` bytes).
fn read_coded_number(d: &[u8], max_len: usize) -> Option<(u64, usize)> {
    let b0 = *d.first()?;
    let (len, mut v) = match b0.leading_ones() {
        0 => (1, u64::from(b0)),
        n @ 2..=7 => (n as usize, u64::from(b0) & (0x7F >> n)),
        _ => return None,
    };
    if len > max_len {
        return None;
    }
    for &b in d.get(1..len)? {
        if b & 0xC0 != 0x80 {
            return None;
        }
        v = (v << 6) | u64::from(b & 0x3F);
    }
    Some((v, len))
}

/// Encode `v` (< 2^36) as a coded number into `out`; returns the length.
pub(crate) fn write_coded_number(v: u64, out: &mut [u8; 7]) -> usize {
    if v < 0x80 {
        out[0] = v as u8;
        return 1;
    }
    // a `len`-byte code carries 5 * len + 1 bits (11, 16, 21, 26, 31, 36)
    let len = (2..=7).find(|&l| v < 1u64 << (5 * l + 1)).unwrap_or(7);
    for i in (1..len).rev() {
        out[i] = 0x80 | ((v >> (6 * (len - 1 - i))) & 0x3F) as u8;
    }
    let lead = (0xFF00u16 >> len) as u8;
    out[0] = lead | (v >> (6 * (len - 1))) as u8;
    len
}

/// The standard sample-rate code for `rate`, if it has one (1..=11).
pub(crate) fn sample_rate_code(rate: u32) -> Option<u8> {
    RATES.iter().position(|&r| r == rate).map(|i| i as u8 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coded_numbers_roundtrip() {
        let mut buf = [0u8; 7];
        for v in [
            0u64, 1, 0x7F, 0x80, 0x7FF, 0x800, 0xFFFF, 0x1_0000, 0x1F_FFFF, 0x20_0000,
        ]
        .into_iter()
        .chain([
            0x3FF_FFFF,
            0x400_0000,
            0x7FFF_FFFF,
            0x8000_0000,
            (1 << 36) - 1,
        ]) {
            let n = write_coded_number(v, &mut buf);
            let expect = match v {
                0..=0x7F => 1,
                0x80..=0x7FF => 2,
                0x800..=0xFFFF => 3,
                0x1_0000..=0x1F_FFFF => 4,
                0x20_0000..=0x3FF_FFFF => 5,
                0x400_0000..=0x7FFF_FFFF => 6,
                _ => 7,
            };
            assert_eq!(n, expect, "{v:#x}");
            assert_eq!(read_coded_number(&buf[..n], 7), Some((v, n)));
        }
        assert_eq!(read_coded_number(&[0x80], 7), None);
        assert_eq!(read_coded_number(&[0xFF, 0x80], 7), None);
        assert_eq!(read_coded_number(&[0xC2, 0x00], 7), None);
        assert_eq!(
            read_coded_number(&[0xFE, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80], 6),
            None
        );
    }

    #[test]
    fn header_fields() {
        // fixed, block 4096 (code 12), 44.1 kHz (9), stereo independent, 16-bit, frame 0
        let mut h = vec![0xFF, 0xF8, 0xC9, 0x18, 0x00];
        h.push(crc8(&h));
        let fh = parse_frame_header(&h).unwrap();
        assert_eq!(fh.block_size, 4096);
        assert_eq!(fh.sample_rate, Some(44100));
        assert_eq!(fh.channel_assignment, ChannelAssignment::Independent(2));
        assert_eq!(fh.bits_per_sample, Some(16));
        assert_eq!(fh.header_len, 6);
        assert!(!fh.variable_block_size);
        // corrupt CRC
        let mut bad = h.clone();
        bad[5] ^= 1;
        assert!(parse_frame_header(&bad).is_none());
        // variable block size, 16-bit block size field, 16-bit Hz rate, mid-side, bps from SI
        let mut h = vec![0xFF, 0xF9, 0x7D, 0xA0];
        let mut num = [0u8; 7];
        let n = write_coded_number(123_456_789, &mut num);
        h.extend_from_slice(&num[..n]);
        h.extend_from_slice(&[0x03, 0xE7, 0x1F, 0x40]); // 1000 samples, 8000 Hz
        h.push(crc8(&h));
        let fh = parse_frame_header(&h).unwrap();
        assert!(fh.variable_block_size);
        assert_eq!(fh.number, 123_456_789);
        assert_eq!(fh.block_size, 1000);
        assert_eq!(fh.sample_rate, Some(8000));
        assert_eq!(fh.bits_per_sample, None);
        assert_eq!(fh.channel_assignment, ChannelAssignment::MidSide);
        let mut stream = vec![0xFF, 0xF8, 0xFF, 0x00, 0x12];
        stream.extend_from_slice(&h);
        assert_eq!(find_frame_sync(&stream), Some(5));
        // reserved codes
        let edits: [fn(&mut Vec<u8>); 5] = [
            |x| x[2] &= 0x0F,                // block size code 0
            |x| x[2] |= 0x0F,                // sample rate code 15
            |x| x[3] = (x[3] & 0x0F) | 0xB0, // channel code 11
            |x| x[3] = (x[3] & 0xF1) | 0x06, // bps code 3
            |x| x[3] |= 1,                   // reserved bit
        ];
        for (i, edit) in edits.iter().enumerate() {
            let mut x = h.clone();
            edit(&mut x);
            let len = x.len();
            x[len - 1] = crc8(&x[..len - 1]);
            assert!(parse_frame_header(&x).is_none(), "edit {i}");
        }
        for len in 0..h.len() {
            assert!(parse_frame_header(&h[..len]).is_none());
        }
    }
}

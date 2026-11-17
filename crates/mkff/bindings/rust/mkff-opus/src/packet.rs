//! Opus packet framing (RFC 6716 §3): the TOC byte, frame packing codes 0–3, padding and the
//! self-delimited variant used inside multistream packets (RFC 6716 Appendix B).

use mkff_audio_core::{Error, Result};

/// The coding mode of a packet (RFC 6716 §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// SILK only (linear prediction, speech).
    SilkOnly,
    /// SILK below 8 kHz plus CELT above.
    Hybrid,
    /// CELT only (MDCT, music / low delay).
    CeltOnly,
}

/// Audio bandwidth signalled in the TOC byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bandwidth {
    /// 4 kHz (8 kHz sample rate).
    Narrowband,
    /// 6 kHz (12 kHz sample rate).
    Mediumband,
    /// 8 kHz (16 kHz sample rate).
    Wideband,
    /// 12 kHz (24 kHz sample rate).
    SuperWideband,
    /// 20 kHz (48 kHz sample rate).
    Fullband,
}

/// A decoded TOC byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Toc {
    /// The configuration number (0–31).
    pub config: u8,
    /// Coding mode of every frame in the packet.
    pub mode: Mode,
    /// Audio bandwidth.
    pub bandwidth: Bandwidth,
    /// Samples per frame at 48 kHz (120, 240, 480, 960, 1920 or 2880).
    pub frame_samples: usize,
    /// Whether the frames are stereo.
    pub stereo: bool,
    /// The frame count code (0–3).
    pub code: u8,
}

impl Toc {
    /// Decode a TOC byte.
    pub fn parse(toc: u8) -> Toc {
        let config = toc >> 3;
        let (mode, bandwidth, frame_samples) = match config {
            0..=11 => {
                let bw = match config >> 2 {
                    0 => Bandwidth::Narrowband,
                    1 => Bandwidth::Mediumband,
                    _ => Bandwidth::Wideband,
                };
                let fs = [480, 960, 1920, 2880][(config & 3) as usize];
                (Mode::SilkOnly, bw, fs)
            }
            12..=15 => {
                let bw = if config < 14 {
                    Bandwidth::SuperWideband
                } else {
                    Bandwidth::Fullband
                };
                (Mode::Hybrid, bw, if config & 1 == 0 { 480 } else { 960 })
            }
            _ => {
                let bw = match (config - 16) >> 2 {
                    0 => Bandwidth::Narrowband,
                    1 => Bandwidth::Wideband,
                    2 => Bandwidth::SuperWideband,
                    _ => Bandwidth::Fullband,
                };
                (Mode::CeltOnly, bw, 120 << (config & 3))
            }
        };
        Toc {
            config,
            mode,
            bandwidth,
            frame_samples,
            stereo: toc & 4 != 0,
            code: toc & 3,
        }
    }
}

/// The frames of one Opus packet (borrowed from the packet).
#[derive(Clone, Debug)]
pub struct Frames<'a> {
    /// The packet's TOC byte.
    pub toc: Toc,
    /// The compressed frames (each 0–1275 bytes; empty frames mean DTX / loss).
    pub frames: Vec<&'a [u8]>,
    /// Bytes of the packet consumed (differs from the input length only for self-delimited
    /// packets).
    pub consumed: usize,
}

fn parse_size(data: &[u8]) -> Result<(usize, usize)> {
    match data {
        [] => Err(Error::invalid("opus: truncated frame length")),
        [b, ..] if *b < 252 => Ok((usize::from(*b), 1)),
        [_] => Err(Error::invalid("opus: truncated frame length")),
        [b0, b1, ..] => Ok((4 * usize::from(*b1) + usize::from(*b0), 2)),
    }
}

/// Split an Opus packet into its frames (RFC 6716 §3.2).
pub fn parse_packet(data: &[u8]) -> Result<Frames<'_>> {
    parse_impl(data, false)
}

/// Split a self-delimited packet (RFC 6716 Appendix B), as found for all but the last stream of
/// a multistream packet. `consumed` says where the next stream's packet starts.
pub fn parse_self_delimited(data: &[u8]) -> Result<Frames<'_>> {
    parse_impl(data, true)
}

fn parse_impl(data: &[u8], self_delimited: bool) -> Result<Frames<'_>> {
    let bad = |m: &str| Error::invalid(format!("opus: {m}"));
    let (&toc_byte, rest) = data.split_first().ok_or_else(|| bad("empty packet"))?;
    let toc = Toc::parse(toc_byte);
    let mut pos = 1usize;
    let mut len = rest.len();
    let mut sizes = [0usize; 48];
    let count;
    let mut cbr = false;
    let mut last_size = len;
    match toc.code {
        0 => count = 1,
        1 => {
            count = 2;
            cbr = true;
            if !self_delimited {
                if len & 1 != 0 {
                    return Err(bad("code 1 packet with odd payload length"));
                }
                last_size = len / 2;
                sizes[0] = last_size;
            }
        }
        2 => {
            count = 2;
            let (sz, bytes) = parse_size(&data[pos..])?;
            len -= bytes;
            if sz > len {
                return Err(bad("code 2 frame length exceeds packet"));
            }
            sizes[0] = sz;
            pos += bytes;
            last_size = len - sz;
        }
        _ => {
            if len < 1 {
                return Err(bad("code 3 packet without frame count"));
            }
            let ch = data[pos];
            pos += 1;
            len -= 1;
            count = usize::from(ch & 0x3f);
            if count == 0 || toc.frame_samples * count > 5760 {
                return Err(bad("invalid frame count"));
            }
            if ch & 0x40 != 0 {
                // Padding: each 255 byte adds 254 and continues (RFC 8251 §4 form).
                let mut len_i = len as isize;
                loop {
                    if len_i <= 0 {
                        return Err(bad("truncated padding length"));
                    }
                    let p = data[pos];
                    pos += 1;
                    len_i -= 1;
                    len_i -= if p == 255 { 254 } else { isize::from(p) };
                    if p != 255 {
                        break;
                    }
                }
                if len_i < 0 {
                    return Err(bad("padding exceeds packet"));
                }
                len = len_i as usize;
            }
            cbr = ch & 0x80 == 0;
            if !cbr {
                last_size = len;
                for size in sizes.iter_mut().take(count - 1) {
                    let (sz, bytes) = parse_size(&data[pos..data.len().min(pos + len)])?;
                    len -= bytes;
                    if sz > len {
                        return Err(bad("frame length exceeds packet"));
                    }
                    *size = sz;
                    pos += bytes;
                    last_size = last_size
                        .checked_sub(bytes + sz)
                        .ok_or_else(|| bad("frame lengths exceed packet"))?;
                }
            } else if !self_delimited {
                last_size = len / count;
                if last_size * count != len {
                    return Err(bad("CBR payload not divisible by frame count"));
                }
                for size in sizes.iter_mut().take(count - 1) {
                    *size = last_size;
                }
            }
        }
    }
    if self_delimited {
        let (sz, bytes) = parse_size(&data[pos..data.len().min(pos + len)])?;
        len -= bytes;
        if sz > len {
            return Err(bad("self-delimited frame length exceeds packet"));
        }
        pos += bytes;
        if cbr {
            if sz * count > len {
                return Err(bad("self-delimited CBR frames exceed packet"));
            }
            for size in sizes.iter_mut().take(count - 1) {
                *size = sz;
            }
        } else if sz > last_size {
            return Err(bad("self-delimited frame length exceeds packet"));
        }
        sizes[count - 1] = sz;
    } else {
        if last_size > 1275 {
            return Err(bad("frame longer than 1275 bytes"));
        }
        sizes[count - 1] = last_size;
    }
    let mut frames = Vec::with_capacity(count);
    for &sz in &sizes[..count] {
        if sz > 1275 || pos + sz > data.len() {
            return Err(bad("frame exceeds packet"));
        }
        frames.push(&data[pos..pos + sz]);
        pos += sz;
    }
    // Padding (and, for self-delimited packets, nothing else) follows the frames.
    let consumed = if self_delimited { pos } else { data.len() };
    Ok(Frames {
        toc,
        frames,
        consumed,
    })
}

/// Number of samples (per channel, at 48 kHz) a packet decodes to.
///
/// Only the TOC byte and frame count are examined; errors are returned for an empty packet, a
/// missing code 3 frame count, or a duration above 120 ms.
pub fn packet_samples(packet: &[u8]) -> Result<usize> {
    let toc = Toc::parse(*packet.first().ok_or(Error::invalid("opus: empty packet"))?);
    let count = match toc.code {
        0 => 1,
        1 | 2 => 2,
        _ => usize::from(
            packet
                .get(1)
                .ok_or(Error::invalid("opus: missing frame count"))?
                & 0x3f,
        ),
    };
    let n = count * toc.frame_samples;
    if n > 5760 {
        return Err(Error::invalid("opus: packet longer than 120 ms"));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_configs() {
        let t = Toc::parse(0);
        assert_eq!(
            (t.mode, t.bandwidth, t.frame_samples),
            (Mode::SilkOnly, Bandwidth::Narrowband, 480)
        );
        let t = Toc::parse(11 << 3);
        assert_eq!(
            (t.mode, t.bandwidth, t.frame_samples),
            (Mode::SilkOnly, Bandwidth::Wideband, 2880)
        );
        let t = Toc::parse(13 << 3 | 4);
        assert_eq!(
            (t.mode, t.bandwidth, t.frame_samples),
            (Mode::Hybrid, Bandwidth::SuperWideband, 960)
        );
        assert!(t.stereo);
        let t = Toc::parse(14 << 3);
        assert_eq!(
            (t.mode, t.bandwidth, t.frame_samples),
            (Mode::Hybrid, Bandwidth::Fullband, 480)
        );
        let t = Toc::parse(16 << 3);
        assert_eq!(
            (t.mode, t.bandwidth, t.frame_samples),
            (Mode::CeltOnly, Bandwidth::Narrowband, 120)
        );
        let t = Toc::parse(31 << 3 | 3);
        assert_eq!(
            (t.mode, t.bandwidth, t.frame_samples),
            (Mode::CeltOnly, Bandwidth::Fullband, 960)
        );
        assert_eq!(t.code, 3);
        // All 32 configurations: durations at 48 kHz.
        let want = [
            480, 960, 1920, 2880, 480, 960, 1920, 2880, 480, 960, 1920, 2880, 480, 960, 480, 960,
            120, 240, 480, 960, 120, 240, 480, 960, 120, 240, 480, 960, 120, 240, 480, 960,
        ];
        for (c, w) in want.iter().enumerate() {
            assert_eq!(Toc::parse((c as u8) << 3).frame_samples, *w, "config {c}");
        }
    }

    #[test]
    fn code0() {
        let p = [0xf8u8, 1, 2, 3];
        let f = parse_packet(&p).unwrap();
        assert_eq!(f.frames, vec![&p[1..]]);
        assert_eq!(packet_samples(&p).unwrap(), 960);
        // Empty frame (DTX).
        assert_eq!(parse_packet(&[0xf8]).unwrap().frames, vec![&[][..]]);
    }

    #[test]
    fn code1() {
        let p = [0xf9u8, 1, 2, 3, 4];
        let f = parse_packet(&p).unwrap();
        assert_eq!(f.frames, vec![&p[1..3], &p[3..5]]);
        assert_eq!(packet_samples(&p).unwrap(), 1920);
        assert!(parse_packet(&[0xf9, 1, 2, 3]).is_err());
    }

    #[test]
    fn code2() {
        let p = [0xfau8, 1, 9, 7, 7];
        let f = parse_packet(&p).unwrap();
        assert_eq!(f.frames, vec![&p[2..3], &p[3..5]]);
        // Two-byte length: 252 + 4*1 = 256 bytes.
        let mut p = vec![0xfa, 252, 1];
        p.extend(std::iter::repeat_n(0u8, 300));
        let f = parse_packet(&p).unwrap();
        assert_eq!(f.frames[0].len(), 256);
        assert_eq!(f.frames[1].len(), 44);
        assert!(parse_packet(&[0xfa, 5, 1, 2]).is_err());
        assert!(parse_packet(&[0xfa]).is_err());
    }

    #[test]
    fn code3_cbr_vbr_padding() {
        // CBR, 3 frames of 2 bytes.
        let p = [0xfbu8, 0x03, 1, 1, 2, 2, 3, 3];
        let f = parse_packet(&p).unwrap();
        assert_eq!(f.frames.len(), 3);
        assert_eq!(f.frames[2], &[3, 3]);
        assert_eq!(packet_samples(&p).unwrap(), 2880);
        // VBR with padding: 2 frames, first 1 byte, padding 2 bytes.
        let p = [0xfbu8, 0xc2, 2, 1, 9, 8, 8, 0, 0];
        let f = parse_packet(&p).unwrap();
        assert_eq!(f.frames, vec![&[9u8][..], &[8u8, 8][..]]);
        // Padding of 254 + 1 bytes via a 255 byte.
        let mut p = vec![0x03u8 << 3 | 3, 0x41, 255, 1];
        p.extend([5u8; 4]);
        p.extend([0u8; 255]);
        let f = parse_packet(&p).unwrap();
        assert_eq!(f.frames, vec![&[5u8, 5, 5, 5][..]]);
        // Errors: zero frames, too long, padding overflow, non-divisible CBR.
        assert!(parse_packet(&[0xfb, 0x00]).is_err());
        assert!(parse_packet(&[0xfb, 0x31]).is_err()); // 49 x 20 ms
        assert!(parse_packet(&[0xfb, 0x41, 200, 1, 2]).is_err());
        assert!(parse_packet(&[0xfb, 0x02, 1, 2, 3]).is_err());
        assert!(parse_packet(&[0xfb]).is_err());
        // 120 ms is the maximum.
        assert!(parse_packet(&[0x03 << 3 | 3, 0x02]).is_ok()); // 2 x 60 ms SILK, empty frames
        assert!(packet_samples(&[0x03 << 3 | 3, 0x03]).is_err());
    }

    #[test]
    fn self_delimited() {
        // Code 0 with an explicit length, followed by another packet.
        let p = [0xf8u8, 2, 7, 7, 0xf8, 1];
        let f = parse_self_delimited(&p).unwrap();
        assert_eq!(f.frames, vec![&[7u8, 7][..]]);
        assert_eq!(f.consumed, 4);
        // Code 1: one length for both frames.
        let p = [0xf9u8, 1, 4, 5, 9];
        let f = parse_self_delimited(&p).unwrap();
        assert_eq!(f.frames, vec![&[4u8][..], &[5u8][..]]);
        assert_eq!(f.consumed, 4);
        assert!(parse_self_delimited(&[0xf8, 5, 1]).is_err());
    }

    #[test]
    fn long_frames_rejected() {
        let mut p = vec![0xf8u8];
        p.extend(std::iter::repeat_n(0u8, 1276));
        assert!(parse_packet(&p).is_err());
    }
}

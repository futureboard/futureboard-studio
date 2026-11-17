//! ISO base media file format (MP4 / M4A / MOV / 3GP) audio tracks: reading and writing.

mod demux;
mod mux;

pub(crate) use demux::Mp4Demuxer;
pub use mux::{M4aTrack, M4aWriter};

use mkff_audio_core::bits::BitReader;
use mkff_audio_core::{Error, Result};

use crate::adts::AAC_RATES;
use crate::io::Bytes;

/// Iterates over the boxes in a byte slice: `(type, body)`. Stops at the first malformed
/// header; a box whose size runs past the end is cut to the data present.
pub(crate) struct Boxes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Boxes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl<'a> Iterator for Boxes<'a> {
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let d = self.data.get(self.pos..)?;
        if d.len() < 8 {
            return None;
        }
        let size = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as u64;
        let typ = [d[4], d[5], d[6], d[7]];
        let (hlen, size) = match size {
            0 => (8, d.len() as u64),
            1 => {
                if d.len() < 16 {
                    return None;
                }
                (
                    16,
                    u64::from_be_bytes([d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]]),
                )
            }
            s => (8, s),
        };
        if size < hlen as u64 {
            self.pos = self.data.len();
            return None;
        }
        let end = (size.min(d.len() as u64)) as usize;
        self.pos += end;
        Some((typ, &d[hlen..end]))
    }
}

/// The body of the first child box of type `t`.
pub(crate) fn child<'a>(data: &'a [u8], t: &[u8; 4]) -> Option<&'a [u8]> {
    Boxes::new(data).find(|(ty, _)| ty == t).map(|(_, b)| b)
}

/// Follow a path of box types.
pub(crate) fn path<'a>(data: &'a [u8], p: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let mut d = data;
    for t in p {
        d = child(d, t)?;
    }
    Some(d)
}

/// What an `AudioSpecificConfig` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AscInfo {
    pub aot: u32,
    pub core_rate: u32,
    /// Output rate (SBR rate when SBR is signalled).
    pub rate: u32,
    pub channels: u32,
    pub sbr: bool,
    pub ps: bool,
}

impl AscInfo {
    pub fn name(&self) -> &'static str {
        match self.aot {
            1 => "AAC Main",
            2 if self.ps => "HE-AACv2",
            2 if self.sbr => "HE-AAC",
            2 => "AAC-LC",
            3 => "AAC SSR",
            4 => "AAC LTP",
            23 => "AAC LD",
            39 => "AAC ELD",
            42 => "xHE-AAC",
            _ => "AAC",
        }
    }
}

fn read_aot(br: &mut BitReader) -> Result<u32> {
    let a = br.read(5)?;
    Ok(if a == 31 { 32 + br.read(6)? } else { a })
}

fn read_rate(br: &mut BitReader) -> Result<u32> {
    let i = br.read(4)?;
    if i == 15 {
        br.read(24)
    } else {
        AAC_RATES
            .get(i as usize)
            .copied()
            .ok_or_else(|| Error::invalid("bad AAC sampling frequency index"))
    }
}

/// Parse an `AudioSpecificConfig` (ISO/IEC 14496-3 §1.6.2.1) far enough to know the object
/// type, rates, channels and SBR / PS signalling (explicit and backward-compatible).
pub(crate) fn parse_asc(asc: &[u8]) -> Result<AscInfo> {
    let mut br = BitReader::new(asc);
    let mut aot = read_aot(&mut br)?;
    let core_rate = read_rate(&mut br)?;
    let chcfg = br.read(4)?;
    let mut sbr = false;
    let mut ps = false;
    let mut rate = core_rate;
    let mut explicit = false;
    if aot == 5 || aot == 29 {
        explicit = true;
        sbr = true;
        ps = aot == 29;
        rate = read_rate(&mut br)?;
        aot = read_aot(&mut br)?;
    }
    let channels = match chcfg {
        1..=6 => chcfg,
        7 | 12 | 14 => 8,
        11 => 7,
        13 => 24,
        _ => 0,
    };
    let ga = matches!(aot, 1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23);
    if ga && chcfg != 0 && !explicit {
        // GASpecificConfig, then look for the backward-compatible SBR / PS extension
        let r = (|| -> Result<()> {
            br.read(1)?; // frameLengthFlag
            if br.read(1)? == 1 {
                br.read(14)?; // coreCoderDelay
            }
            let ext = br.read(1)?;
            if aot == 6 || aot == 20 {
                br.read(3)?;
            }
            if ext == 1 {
                if aot == 22 {
                    br.read(16)?;
                }
                if matches!(aot, 17 | 19 | 20 | 23) {
                    br.read(3)?;
                }
                br.read(1)?;
            }
            if br.remaining() >= 16 && br.read(11)? == 0x2B7 {
                let ext_aot = read_aot(&mut br)?;
                if ext_aot == 5 && br.read(1)? == 1 {
                    sbr = true;
                    rate = read_rate(&mut br)?;
                    if br.remaining() >= 12 && br.read(11)? == 0x548 {
                        ps = br.read(1)? == 1;
                    }
                }
            }
            Ok(())
        })();
        let _ = r;
    }
    let channels = if ps && channels == 1 { 2 } else { channels };
    Ok(AscInfo {
        aot,
        core_rate,
        rate,
        channels,
        sbr,
        ps,
    })
}

/// Read an MPEG-4 descriptor header (tag, length) from `b`.
fn descriptor<'a>(b: &mut Bytes<'a>) -> Result<(u8, &'a [u8])> {
    let tag = b.u8()?;
    let mut len = 0usize;
    for _ in 0..4 {
        let c = b.u8()?;
        len = (len << 7) | usize::from(c & 0x7F);
        if c & 0x80 == 0 {
            break;
        }
    }
    let len = len.min(b.remaining());
    Ok((tag, b.bytes(len)?))
}

/// The decoder configuration in an `esds` box (after its version / flags).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Esds {
    pub oti: u8,
    pub max_bitrate: u32,
    pub avg_bitrate: u32,
    pub dsi: Vec<u8>,
}

pub(crate) fn parse_esds(body: &[u8]) -> Result<Esds> {
    let mut b = Bytes::new(body.get(4..).unwrap_or(&[]));
    let (tag, es) = descriptor(&mut b)?;
    let mut out = Esds::default();
    let dc = if tag == 0x03 {
        let mut e = Bytes::new(es);
        e.u16()?;
        let flags = e.u8()?;
        if flags & 0x80 != 0 {
            e.skip(2)?;
        }
        if flags & 0x40 != 0 {
            let n = e.u8()?;
            e.skip(usize::from(n))?;
        }
        if flags & 0x20 != 0 {
            e.skip(2)?;
        }
        let mut found = None;
        while e.remaining() > 0 {
            let (t, d) = descriptor(&mut e)?;
            if t == 0x04 {
                found = Some(d);
                break;
            }
        }
        found.ok_or_else(|| Error::invalid("esds without DecoderConfigDescriptor"))?
    } else if tag == 0x04 {
        es
    } else {
        return Err(Error::invalid("bad esds"));
    };
    let mut d = Bytes::new(dc);
    out.oti = d.u8()?;
    d.u8()?;
    d.u24()?;
    out.max_bitrate = d.u32()?;
    out.avg_bitrate = d.u32()?;
    while d.remaining() > 0 {
        let Ok((t, x)) = descriptor(&mut d) else {
            break;
        };
        if t == 0x05 {
            out.dsi = x.to_vec();
            break;
        }
    }
    Ok(out)
}

/// Channels described by an AC-3 `acmod` + `lfeon`.
pub(crate) fn ac3_channels(acmod: u32, lfe: u32) -> u32 {
    [2, 1, 2, 3, 3, 4, 4, 5][(acmod & 7) as usize] + lfe
}

/// (sample rate, channels) from a `dac3` payload (ETSI TS 102 366 F.4).
pub(crate) fn parse_dac3(d: &[u8]) -> Result<(u32, u32)> {
    let mut br = BitReader::new(d);
    let fscod = br.read(2)?;
    br.read(5)?;
    br.read(3)?;
    let acmod = br.read(3)?;
    let lfe = br.read(1)?;
    let rate = [48000, 44100, 32000, 0][fscod as usize];
    Ok((rate, ac3_channels(acmod, lfe)))
}

/// (sample rate, channels) from a `dec3` payload (ETSI TS 102 366 F.6): the first independent
/// substream plus the channels its dependent substreams add.
pub(crate) fn parse_dec3(d: &[u8]) -> Result<(u32, u32)> {
    let mut br = BitReader::new(d);
    br.read(13)?;
    let n = br.read(3)? + 1;
    let mut out = None;
    for _ in 0..n {
        let fscod = br.read(2)?;
        br.read(5)?; // bsid
        br.read(1)?;
        br.read(1)?; // asvc
        br.read(3)?; // bsmod
        let acmod = br.read(3)?;
        let lfe = br.read(1)?;
        br.read(3)?;
        let deps = br.read(4)?;
        let mut ch = ac3_channels(acmod, lfe);
        if deps > 0 {
            let loc = br.read(9)?;
            // Lc/Rc, Lrs/Rrs, Cs, Ts, Lsd/Rsd, Lw/Rw, Lvh/Rvh, Cvh, LFE2
            const ADD: [u32; 9] = [2, 2, 1, 1, 2, 2, 2, 1, 1];
            for (i, a) in ADD.iter().enumerate() {
                if loc >> (8 - i) & 1 == 1 {
                    ch += a;
                }
            }
        } else {
            br.read(1)?;
        }
        if out.is_none() {
            out = Some(([48000, 44100, 32000, 0][fscod as usize], ch));
        }
    }
    out.ok_or_else(|| Error::invalid("empty dec3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_variants() {
        // AAC-LC 44.1 kHz stereo
        let a = parse_asc(&[0x12, 0x10]).unwrap();
        assert_eq!((a.aot, a.rate, a.channels, a.sbr), (2, 44100, 2, false));
        // explicit HE-AAC: AOT 5, 24 kHz core, 48 kHz SBR, stereo, core AOT 2
        let a = parse_asc(&[0x2B, 0x11, 0x88, 0x00]).unwrap();
        assert_eq!(
            (a.aot, a.core_rate, a.rate, a.channels, a.sbr),
            (2, 24000, 48000, 2, true)
        );
        assert_eq!(a.name(), "HE-AAC");
        // backward-compatible signalling: LC 22.05 kHz mono + 0x2B7 / AOT 5 / SBR 44.1 kHz
        let a = parse_asc(&[0x13, 0x88, 0x56, 0xE5, 0xA0]).unwrap();
        assert!(a.sbr, "{a:?}");
        assert_eq!(a.rate, 44100);
        assert!(parse_asc(&[0x12]).is_err());
    }

    #[test]
    fn esds_parse() {
        let body = [
            0, 0, 0, 0, 0x03, 0x19, 0x00, 0x01, 0x00, 0x04, 0x11, 0x40, 0x15, 0x00, 0x01, 0x00,
            0x00, 0x01, 0xF4, 0x00, 0x00, 0x01, 0xF4, 0x00, 0x05, 0x02, 0x12, 0x10, 0x06, 0x01,
            0x02,
        ];
        let e = parse_esds(&body).unwrap();
        assert_eq!(e.oti, 0x40);
        assert_eq!(e.dsi, vec![0x12, 0x10]);
        assert_eq!(e.avg_bitrate, 0x1F400);
    }

    #[test]
    fn dac3_dec3() {
        // 48 kHz, acmod 7 (3/2), LFE on
        let (r, c) = parse_dac3(&[0x10, 0x3D, 0x40]).unwrap();
        assert_eq!((r, c), (48000, 6));
        assert!(parse_dec3(&[0x00]).is_err());
    }
}

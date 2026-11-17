//! AIFF / AIFF-C reading.

use mkff_audio_core::{CodecId, Error, Result};

use crate::io::{be32, Bytes, Source};
use crate::pcm::{PcmCoding, PcmFormat};
use crate::wav::PcmRegion;
use crate::Track;

/// Decode an IEEE 754 80-bit extended float (big-endian), as used for the AIFF sample rate.
pub(crate) fn extended_to_f64(b: &[u8; 10]) -> f64 {
    let se = u16::from_be_bytes([b[0], b[1]]);
    let mant = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    let exp = i32::from(se & 0x7FFF);
    if exp == 0 && mant == 0 {
        return 0.0;
    }
    if exp == 0x7FFF {
        return f64::NAN;
    }
    let v = mant as f64 * 2f64.powi(exp - 16383 - 63);
    if se & 0x8000 != 0 {
        -v
    } else {
        v
    }
}

/// Encode a positive rate as an 80-bit extended float.
#[cfg(test)]
pub(crate) fn f64_to_extended(v: f64) -> [u8; 10] {
    let mut out = [0u8; 10];
    if v <= 0.0 {
        return out;
    }
    let mut exp = 16383 + 63;
    let mut m = v;
    while m >= 18_446_744_073_709_551_616.0 {
        m /= 2.0;
        exp += 1;
    }
    while m < 9_223_372_036_854_775_808.0 {
        m *= 2.0;
        exp -= 1;
    }
    out[..2].copy_from_slice(&(exp as u16).to_be_bytes());
    out[2..].copy_from_slice(&(m as u64).to_be_bytes());
    out
}

pub(crate) struct AiffDemuxer;

impl AiffDemuxer {
    #[allow(clippy::new_ret_no_self)]
    pub fn open(mut src: Source) -> Result<PcmRegion> {
        let mut h = [0u8; 12];
        src.read_at(0, &mut h)?;
        if &h[..4] != b"FORM" || !(&h[8..12] == b"AIFF" || &h[8..12] == b"AIFC") {
            return Err(Error::invalid("not an AIFF file"));
        }
        let aifc = &h[8..12] == b"AIFC";
        let len = src.len();
        let form_end = (8 + u64::from(be32(&h, 4))).min(len);
        let mut pos = 12u64;
        let mut comm: Option<(u32, u32, u32, f64, [u8; 4])> = None;
        let mut ssnd: Option<(u64, u64)> = None;
        while pos + 8 <= len {
            let mut ck = [0u8; 8];
            if src.read_at(pos, &mut ck)? < 8 {
                break;
            }
            let size = u64::from(be32(&ck, 4));
            match &ck[..4] {
                b"COMM" => {
                    let body = src.read_vec(size.min(1024).min(len - pos - 8))?;
                    let mut b = Bytes::new(&body);
                    let channels = u32::from(b.u16()?);
                    let frames = b.u32()?;
                    let bits = u32::from(b.u16()?);
                    let rate = extended_to_f64(&b.array()?);
                    let comp = if aifc {
                        b.array().unwrap_or(*b"NONE")
                    } else {
                        *b"NONE"
                    };
                    comm = Some((channels, frames, bits, rate, comp));
                }
                b"SSND" => {
                    let mut s = [0u8; 8];
                    src.read_at(pos + 8, &mut s)?;
                    let offset = u64::from(be32(&s, 0));
                    let start = pos + 16 + offset;
                    let avail = if pos + 8 + size > form_end.max(len) || size < 8 {
                        len.saturating_sub(start)
                    } else {
                        (pos + 8 + size).saturating_sub(start)
                    };
                    ssnd = Some((start, avail));
                    if comm.is_some() {
                        break;
                    }
                }
                _ => {}
            }
            let next = pos.saturating_add(8 + size + (size & 1));
            if next <= pos {
                break;
            }
            pos = next;
        }
        let (channels, frames, bits, rate, comp) =
            comm.ok_or_else(|| Error::invalid("AIFF without COMM"))?;
        let (start, avail) = ssnd.ok_or_else(|| Error::invalid("AIFF without SSND"))?;
        if channels == 0 || !(1.0..=10_000_000.0).contains(&rate) {
            return Err(Error::invalid("AIFF with 0 channels or a bad rate"));
        }
        let slot = bits.clamp(1, 32).div_ceil(8) * 8;
        let fmt = match &comp {
            b"NONE" | b"twos" | b"TWOS" => PcmFormat::int(slot, true),
            b"sowt" | b"SOWT" => PcmFormat::int(slot, false),
            b"fl32" | b"FL32" => PcmFormat::float(32, true),
            b"fl64" | b"FL64" => PcmFormat::float(64, true),
            b"in24" | b"IN24" => PcmFormat::int(24, true),
            b"in32" | b"IN32" => PcmFormat::int(32, true),
            b"raw " => PcmFormat::u8(),
            b"alaw" | b"ALAW" => PcmFormat::alaw(),
            b"ulaw" | b"ULAW" => PcmFormat::mulaw(),
            c => {
                return Err(Error::unsupported(format!(
                    "AIFF-C compression '{}'",
                    String::from_utf8_lossy(c)
                )))
            }
        };
        let fmt = if fmt.bits == 8 && fmt.coding == PcmCoding::Linear && &comp != b"raw " {
            PcmFormat::int(8, true)
        } else {
            fmt
        };
        if !fmt.is_valid() {
            return Err(Error::unsupported("AIFF sample size"));
        }
        let mut t = Track::new(
            CodecId::Pcm,
            rate.round() as u32,
            channels,
            format!("PCM {}", fmt.label()),
        );
        t.pcm = Some(fmt);
        t.bits_per_sample = Some(if fmt.coding == PcmCoding::Linear && !fmt.float {
            bits.min(fmt.bits)
        } else {
            fmt.bits
        });
        let block = fmt.bytes_per_sample() as u64 * u64::from(channels);
        let data_len = if frames > 0 {
            (u64::from(frames) * block).min(avail)
        } else {
            avail
        };
        PcmRegion::new(src, "AIFF", t, start, data_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_rates() {
        for r in [
            8000.0, 11025.0, 22050.0, 44100.0, 48000.0, 96000.0, 192000.0, 1.0,
        ] {
            assert_eq!(extended_to_f64(&f64_to_extended(r)), r);
        }
        // 44100 as written by Apple tools
        let b = [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0];
        assert_eq!(extended_to_f64(&b), 44100.0);
    }
}

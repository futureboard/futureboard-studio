//! PCM sample formats, decoding to planar f32 and encoding from it.

use mkff_audio_core::{AudioBuffer, Decoder, Error, Result};

/// How PCM samples are coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum PcmCoding {
    /// Linear integer or float samples.
    #[default]
    Linear,
    /// ITU-T G.711 A-law (8 bits per sample).
    ALaw,
    /// ITU-T G.711 µ-law (8 bits per sample).
    MuLaw,
}

/// The layout of interleaved PCM samples in a container.
///
/// `bits` is the *container* size of one sample (8, 16, 24, 32 or 64); the number of
/// significant bits, when smaller (e.g. 20-bit audio in 24-bit slots), is
/// [`Track::bits_per_sample`](crate::Track::bits_per_sample). Samples are interleaved frame by
/// frame (L R L R …).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PcmFormat {
    /// Bits per sample slot: 8, 16, 24 or 32 for integers, 32 or 64 for floats, 8 for G.711.
    pub bits: u32,
    /// IEEE 754 floating point samples.
    pub float: bool,
    /// Big-endian byte order (ignored for 8-bit samples).
    pub big_endian: bool,
    /// Two's complement samples; unsigned samples are offset binary (8-bit WAV).
    pub signed: bool,
    /// Linear or G.711 companded.
    pub coding: PcmCoding,
}

impl PcmFormat {
    /// Signed integer samples of `bits` (8 / 16 / 24 / 32).
    pub const fn int(bits: u32, big_endian: bool) -> Self {
        Self {
            bits,
            float: false,
            big_endian,
            signed: true,
            coding: PcmCoding::Linear,
        }
    }

    /// Unsigned 8-bit samples (WAV 8-bit).
    pub const fn u8() -> Self {
        Self {
            bits: 8,
            float: false,
            big_endian: false,
            signed: false,
            coding: PcmCoding::Linear,
        }
    }

    /// IEEE float samples of `bits` (32 / 64).
    pub const fn float(bits: u32, big_endian: bool) -> Self {
        Self {
            bits,
            float: true,
            big_endian,
            signed: true,
            coding: PcmCoding::Linear,
        }
    }

    /// G.711 A-law.
    pub const fn alaw() -> Self {
        Self {
            bits: 8,
            float: false,
            big_endian: false,
            signed: true,
            coding: PcmCoding::ALaw,
        }
    }

    /// G.711 µ-law.
    pub const fn mulaw() -> Self {
        Self {
            bits: 8,
            float: false,
            big_endian: false,
            signed: true,
            coding: PcmCoding::MuLaw,
        }
    }

    /// Signed 16-bit little-endian (the common WAV format).
    pub const fn s16le() -> Self {
        Self::int(16, false)
    }

    /// Bytes per sample slot.
    pub fn bytes_per_sample(&self) -> usize {
        (self.bits / 8) as usize
    }

    /// Whether this combination is one [`decode_pcm`] supports.
    pub fn is_valid(&self) -> bool {
        match self.coding {
            PcmCoding::ALaw | PcmCoding::MuLaw => self.bits == 8,
            PcmCoding::Linear if self.float => self.bits == 32 || self.bits == 64,
            PcmCoding::Linear => matches!(self.bits, 8 | 16 | 24 | 32),
        }
    }

    /// A short label such as `"s16le"`, `"f32be"`, `"u8"`, `"alaw"`.
    pub fn label(&self) -> String {
        match self.coding {
            PcmCoding::ALaw => "alaw".into(),
            PcmCoding::MuLaw => "mulaw".into(),
            PcmCoding::Linear => {
                let k = if self.float {
                    'f'
                } else if self.signed {
                    's'
                } else {
                    'u'
                };
                if self.bits == 8 {
                    format!("{k}8")
                } else {
                    format!(
                        "{k}{}{}",
                        self.bits,
                        if self.big_endian { "be" } else { "le" }
                    )
                }
            }
        }
    }
}

/// G.711 A-law to 16-bit linear.
fn alaw_to_linear(a: u8) -> i16 {
    let a = a ^ 0x55;
    let mut t = i32::from(a & 0x0F) << 4;
    let seg = (a & 0x70) >> 4;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    (if a & 0x80 != 0 { t } else { -t }) as i16
}

/// G.711 µ-law to 16-bit linear.
fn mulaw_to_linear(u: u8) -> i16 {
    let u = !u;
    let mut t = (i32::from(u & 0x0F) << 3) + 0x84;
    t <<= (u & 0x70) >> 4;
    (if u & 0x80 != 0 { 0x84 - t } else { t - 0x84 }) as i16
}

#[inline]
fn sample(b: &[u8], fmt: &PcmFormat) -> f32 {
    match fmt.coding {
        PcmCoding::ALaw => return f32::from(alaw_to_linear(b[0])) / 32768.0,
        PcmCoding::MuLaw => return f32::from(mulaw_to_linear(b[0])) / 32768.0,
        PcmCoding::Linear => {}
    }
    if fmt.float {
        return if fmt.bits == 32 {
            let a = [b[0], b[1], b[2], b[3]];
            if fmt.big_endian {
                f32::from_be_bytes(a)
            } else {
                f32::from_le_bytes(a)
            }
        } else {
            let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
            (if fmt.big_endian {
                f64::from_be_bytes(a)
            } else {
                f64::from_le_bytes(a)
            }) as f32
        };
    }
    let n = fmt.bytes_per_sample();
    let mut v: u32 = 0;
    for i in 0..n {
        let byte = if fmt.big_endian { b[i] } else { b[n - 1 - i] };
        v = (v << 8) | u32::from(byte);
    }
    let bits = fmt.bits;
    let s = if fmt.signed {
        ((v << (32 - bits)) as i32) >> (32 - bits)
    } else {
        v as i32 - (1i32 << (bits - 1))
    };
    (f64::from(s) / f64::from(1u32 << (bits - 1))) as f32
}

/// Decode interleaved PCM bytes into planar f32 (`sample_rate` is left 0; [`PcmDecoder`] fills
/// it). A trailing partial frame is ignored; an invalid format or 0 channels gives an empty
/// buffer.
pub fn decode_pcm(data: &[u8], fmt: PcmFormat, channels: usize) -> AudioBuffer {
    if !fmt.is_valid() || channels == 0 {
        return AudioBuffer::empty(0, channels);
    }
    let bps = fmt.bytes_per_sample();
    let frame = bps * channels;
    let frames = data.len() / frame;
    let mut out = AudioBuffer::new(0, channels, frames);
    for (i, f) in data.chunks_exact(frame).enumerate() {
        for (c, s) in f.chunks_exact(bps).enumerate() {
            out.channels[c][i] = sample(s, &fmt);
        }
    }
    out
}

/// Encode planar f32 samples as interleaved PCM in `fmt` (integers are rounded and clipped;
/// G.711 is not supported for encoding and gives an error).
pub fn encode_pcm(buf: &AudioBuffer, fmt: PcmFormat) -> Result<Vec<u8>> {
    if !fmt.is_valid() || fmt.coding != PcmCoding::Linear {
        return Err(Error::InvalidArgument(format!(
            "cannot encode PCM as {}",
            fmt.label()
        )));
    }
    let frames = buf.frames();
    let bps = fmt.bytes_per_sample();
    let mut out = Vec::with_capacity(frames * buf.channel_count() * bps);
    for i in 0..frames {
        for ch in &buf.channels {
            let s = ch.get(i).copied().unwrap_or(0.0);
            push_sample(&mut out, s, &fmt);
        }
    }
    Ok(out)
}

pub(crate) fn push_sample(out: &mut Vec<u8>, s: f32, fmt: &PcmFormat) {
    if fmt.float {
        if fmt.bits == 32 {
            out.extend_from_slice(&if fmt.big_endian {
                s.to_be_bytes()
            } else {
                s.to_le_bytes()
            });
        } else {
            let d = f64::from(s);
            out.extend_from_slice(&if fmt.big_endian {
                d.to_be_bytes()
            } else {
                d.to_le_bytes()
            });
        }
        return;
    }
    let bits = fmt.bits;
    let scale = f64::from(1u32 << (bits - 1));
    let max = scale - 1.0;
    let s = if s.is_nan() { 0.0 } else { f64::from(s) };
    let mut v = (s * scale).round().clamp(-scale, max) as i64;
    if !fmt.signed {
        v += scale as i64;
    }
    let n = (bits / 8) as usize;
    for i in 0..n {
        let shift = if fmt.big_endian {
            8 * (n - 1 - i)
        } else {
            8 * i
        };
        out.push((v >> shift) as u8);
    }
}

/// A [`Decoder`] for PCM packets (any packet length that is a whole number of frames).
#[derive(Clone, Debug)]
pub struct PcmDecoder {
    fmt: PcmFormat,
    channels: usize,
    sample_rate: u32,
}

impl PcmDecoder {
    /// A decoder for `fmt` with `channels` interleaved channels at `sample_rate`.
    pub fn new(fmt: PcmFormat, channels: usize, sample_rate: u32) -> Result<Self> {
        if !fmt.is_valid() {
            return Err(Error::unsupported(format!("PCM format {}", fmt.label())));
        }
        if channels == 0 {
            return Err(Error::InvalidArgument("PCM with 0 channels".into()));
        }
        Ok(Self {
            fmt,
            channels,
            sample_rate,
        })
    }

    /// A decoder for a demuxed PCM [`Track`](crate::Track) (`track.pcm` must be set).
    pub fn for_track(track: &crate::Track) -> Result<Self> {
        let fmt = track
            .pcm
            .ok_or_else(|| Error::InvalidArgument("not a PCM track".into()))?;
        Self::new(fmt, track.channels as usize, track.sample_rate)
    }
}

impl Decoder for PcmDecoder {
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        let mut b = decode_pcm(packet, self.fmt, self.channels);
        b.sample_rate = self.sample_rate;
        Ok(b)
    }

    fn reset(&mut self) {}

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> usize {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g711_reference_points() {
        // silence codes
        assert_eq!(mulaw_to_linear(0xFF), 0);
        assert_eq!(mulaw_to_linear(0x7F), 0);
        assert_eq!(alaw_to_linear(0xD5), 8);
        assert_eq!(alaw_to_linear(0x55), -8);
        // full scale
        assert_eq!(mulaw_to_linear(0x80), 32124);
        assert_eq!(mulaw_to_linear(0x00), -32124);
        assert_eq!(alaw_to_linear(0xAA), 32256);
    }

    #[test]
    fn int_round_trip() {
        for fmt in [
            PcmFormat::u8(),
            PcmFormat::int(16, false),
            PcmFormat::int(16, true),
            PcmFormat::int(24, false),
            PcmFormat::int(24, true),
            PcmFormat::int(32, false),
            PcmFormat::float(32, true),
            PcmFormat::float(64, false),
        ] {
            let buf = AudioBuffer {
                sample_rate: 0,
                channels: vec![
                    vec![0.0, 0.5, -0.5, -1.0, 0.25],
                    vec![0.1, -0.1, 0.999, 1.5, 0.0],
                ],
            };
            let bytes = encode_pcm(&buf, fmt).unwrap();
            assert_eq!(bytes.len(), 10 * fmt.bytes_per_sample());
            let back = decode_pcm(&bytes, fmt, 2);
            for (a, b) in buf
                .channels
                .iter()
                .flatten()
                .zip(back.channels.iter().flatten())
            {
                let a = if fmt.float { *a } else { a.clamp(-1.0, 1.0) };
                assert!((a - b).abs() < 0.01, "{} {a} {b}", fmt.label());
            }
        }
    }
}

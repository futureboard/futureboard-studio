//! Opus decoder (SILK, CELT and hybrid), written for MKFF in safe Rust.
//!
//! Implements RFC 6716 (with the RFC 8251 updates) and the Ogg Opus `OpusHead` / channel mapping
//! rules of RFC 7845:
//!
//! - packet framing ([`packet`]: TOC byte, frame codes 0–3, padding, self-delimited framing,
//!   [`packet_samples`]);
//! - the range decoder, the SILK layer (NB / MB / WB, mono / stereo, LBRR skipping, 10–60 ms) with
//!   the reference IIR/FIR resampler to 48 kHz, the CELT layer (energy, allocation, PVQ, spreading,
//!   folding, TF changes, anti-collapse, post-filter, de-emphasis), hybrid frames, mode
//!   transitions and redundancy frames;
//! - multistream packets with channel mapping families 0, 1 and 255 ([`OpusDecoder`]).
//!
//! Output is always 48 kHz planar f32 in WAV channel order. The decoder does not trim the
//! pre-skip ([`OpusDecoder::pre_skip`]); the container layer does.
//!
//! Packet loss (an empty packet) is concealed with ports of the reference SILK PLC and CELT
//! pitch / noise PLC; FEC (LBRR) data is parsed but not used, and comfort noise generation is not
//! implemented.
//!
//! Parts of the algorithms and all tables follow the reference implementation published in
//! RFC 6716 (BSD-3-Clause, see the crate's `NOTICE` file).
//!
//! ```
//! use mkff_audio_core::Decoder;
//! use mkff_opus::{OpusDecoder, OpusHead};
//!
//! # fn main() -> mkff_audio_core::Result<()> {
//! // The configuration is the Ogg / Matroska `OpusHead` packet or an MP4 `dOps` payload.
//! let mut dec = OpusDecoder::from_config(&OpusHead::simple(2).to_bytes())?;
//! // A 20 ms fullband CELT packet (TOC 0xfc: config 31, stereo, one frame).
//! let audio = dec.decode(&[0xfc, 0x12, 0x34, 0x56, 0x78])?;
//! assert_eq!((audio.channel_count(), audio.frames(), audio.sample_rate), (2, 960, 48000));
//! // The container drops `dec.pre_skip()` samples from the start of the stream.
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod celt;
pub mod packet;
mod range;
mod silk;
mod stream;

use mkff_audio_core::{AudioBuffer, Decoder, Error, Result};

pub use packet::{packet_samples, Bandwidth, Mode, Toc};
pub use stream::MAX_PACKET_SAMPLES;

/// Output sample rate of the decoder.
pub const SAMPLE_RATE: u32 = 48000;

/// Stream / coupled counts and mappings of the RFC 7845 §5.1.1.2 (Vorbis order) layouts libopus
/// uses for 1–8 channels.
const VORBIS_LAYOUTS: [(u8, u8, &[u8]); 8] = [
    (1, 0, &[0]),
    (1, 1, &[0, 1]),
    (2, 1, &[0, 2, 1]),
    (2, 2, &[0, 1, 2, 3]),
    (3, 2, &[0, 4, 1, 2, 3]),
    (4, 2, &[0, 4, 1, 2, 3, 5]),
    (4, 3, &[0, 4, 1, 2, 3, 5, 6]),
    (5, 3, &[0, 6, 1, 2, 3, 4, 5, 7]),
];

/// For family 1: the Vorbis-order channel feeding each WAV-order output channel.
fn wav_from_vorbis(channels: usize) -> &'static [usize] {
    match channels {
        3 => &[0, 2, 1],
        5 => &[0, 2, 1, 3, 4],
        6 => &[0, 2, 1, 5, 3, 4],
        7 => &[0, 2, 1, 6, 5, 3, 4],
        8 => &[0, 2, 1, 7, 5, 6, 3, 4],
        _ => &[0, 1, 2, 3, 4, 5, 6, 7],
    }
}

/// The Opus identification header (RFC 7845 §5.1), as found in Ogg (`OpusHead` packet),
/// Matroska / WebM (`CodecPrivate`) and MP4 (`dOps` box).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpusHead {
    /// Output channel count (1–255).
    pub channels: u8,
    /// Samples (at 48 kHz) to discard from the start of the decoded stream.
    pub pre_skip: u16,
    /// Sample rate of the original input (informational only).
    pub input_sample_rate: u32,
    /// Gain to apply to the output, in dB as Q7.8.
    pub output_gain_db_q8: i16,
    /// Channel mapping family (0: mono / stereo, 1: Vorbis surround order, 255: no meaning).
    pub mapping_family: u8,
    /// Number of Opus streams in each packet.
    pub stream_count: u8,
    /// Number of those streams that are stereo (coupled).
    pub coupled_count: u8,
    /// For each output channel, the decoded channel index (`< stream_count + coupled_count`) or
    /// 255 for silence.
    pub mapping: Vec<u8>,
}

impl OpusHead {
    /// Parse an `OpusHead` packet (Ogg identification header / Matroska `CodecPrivate`).
    pub fn parse(data: &[u8]) -> Result<OpusHead> {
        if data.len() < 19 || &data[..8] != b"OpusHead" {
            return Err(Error::invalid("opus: not an OpusHead packet"));
        }
        if data[8] & 0xf0 != 0 {
            return Err(Error::unsupported(format!(
                "opus: OpusHead version {}",
                data[8]
            )));
        }
        let channels = data[9];
        let pre_skip = u16::from_le_bytes([data[10], data[11]]);
        let input_sample_rate = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        let output_gain_db_q8 = i16::from_le_bytes([data[16], data[17]]);
        let family = data[18];
        Self::finish(
            channels,
            pre_skip,
            input_sample_rate,
            output_gain_db_q8,
            family,
            &data[19..],
        )
    }

    /// Parse the payload of an MP4 `dOps` box (`OpusSpecificBox`, big-endian fields).
    pub fn from_dops(data: &[u8]) -> Result<OpusHead> {
        if data.len() < 11 {
            return Err(Error::invalid("opus: dOps box too short"));
        }
        if data[0] != 0 {
            return Err(Error::unsupported(format!(
                "opus: dOps version {}",
                data[0]
            )));
        }
        let channels = data[1];
        let pre_skip = u16::from_be_bytes([data[2], data[3]]);
        let input_sample_rate = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let output_gain_db_q8 = i16::from_be_bytes([data[8], data[9]]);
        let family = data[10];
        Self::finish(
            channels,
            pre_skip,
            input_sample_rate,
            output_gain_db_q8,
            family,
            &data[11..],
        )
    }

    fn finish(
        channels: u8,
        pre_skip: u16,
        input_sample_rate: u32,
        output_gain_db_q8: i16,
        family: u8,
        rest: &[u8],
    ) -> Result<OpusHead> {
        if channels == 0 {
            return Err(Error::invalid("opus: zero channels"));
        }
        let (stream_count, coupled_count, mapping) = if family == 0 {
            if channels > 2 {
                return Err(Error::invalid(
                    "opus: mapping family 0 with more than 2 channels",
                ));
            }
            (1, channels - 1, (0..channels).collect::<Vec<u8>>())
        } else {
            if rest.len() < 2 + usize::from(channels) {
                return Err(Error::invalid("opus: truncated channel mapping table"));
            }
            (
                rest[0],
                rest[1],
                rest[2..2 + usize::from(channels)].to_vec(),
            )
        };
        if family == 1 && channels > 8 {
            return Err(Error::invalid(
                "opus: mapping family 1 with more than 8 channels",
            ));
        }
        if stream_count == 0 || coupled_count > stream_count {
            return Err(Error::invalid("opus: bad stream counts"));
        }
        let decoded = u32::from(stream_count) + u32::from(coupled_count);
        if decoded > 255 {
            return Err(Error::invalid("opus: too many decoded channels"));
        }
        if mapping.iter().any(|&m| m != 255 && u32::from(m) >= decoded) {
            return Err(Error::invalid("opus: channel mapping index out of range"));
        }
        Ok(OpusHead {
            channels,
            pre_skip,
            input_sample_rate,
            output_gain_db_q8,
            mapping_family: family,
            stream_count,
            coupled_count,
            mapping,
        })
    }

    /// A header for `channels` channels with the layouts libopus uses: family 0 for mono /
    /// stereo, family 1 (Vorbis surround layouts) for 3–8 channels, family 255 with one mono
    /// stream per channel above that. Pre-skip 312, 48 kHz, no gain.
    pub fn simple(channels: u8) -> OpusHead {
        let channels = channels.max(1);
        let (family, streams, coupled, mapping) = match channels {
            1 | 2 => (0, 1, channels - 1, (0..channels).collect()),
            3..=8 => {
                let (s, c, m) = VORBIS_LAYOUTS[usize::from(channels) - 1];
                (1, s, c, m.to_vec())
            }
            _ => (255, channels, 0, (0..channels).collect()),
        };
        OpusHead {
            channels,
            pre_skip: 312,
            input_sample_rate: 48000,
            output_gain_db_q8: 0,
            mapping_family: family,
            stream_count: streams,
            coupled_count: coupled,
            mapping,
        }
    }

    /// Serialise as an `OpusHead` packet.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = b"OpusHead".to_vec();
        v.push(1);
        v.push(self.channels);
        v.extend_from_slice(&self.pre_skip.to_le_bytes());
        v.extend_from_slice(&self.input_sample_rate.to_le_bytes());
        v.extend_from_slice(&self.output_gain_db_q8.to_le_bytes());
        v.push(self.mapping_family);
        if self.mapping_family != 0 {
            v.push(self.stream_count);
            v.push(self.coupled_count);
            v.extend_from_slice(&self.mapping);
        }
        v
    }
}

/// The Opus decoder: one or more streams (RFC 7845 multistream), output at 48 kHz, planar f32
/// in WAV channel order, with the header's output gain applied.
pub struct OpusDecoder {
    head: OpusHead,
    streams: Vec<stream::StreamDecoder>,
    gain: f32,
    /// For each output (WAV order) channel: `Some((stream, channel in stream))` or silence.
    sources: Vec<Option<(usize, usize)>>,
    scratch: Vec<Vec<f32>>,
    last_samples: usize,
}

impl OpusDecoder {
    /// A decoder for the streams described by `head`.
    pub fn new(head: &OpusHead) -> Result<Self> {
        let head = OpusHead::finish(
            head.channels,
            head.pre_skip,
            head.input_sample_rate,
            head.output_gain_db_q8,
            head.mapping_family,
            &[&[head.stream_count, head.coupled_count][..], &head.mapping].concat(),
        )?;
        let streams: Vec<stream::StreamDecoder> = (0..usize::from(head.stream_count))
            .map(|s| {
                stream::StreamDecoder::new(if s < usize::from(head.coupled_count) {
                    2
                } else {
                    1
                })
            })
            .collect();
        let nc = usize::from(head.channels);
        let coupled = usize::from(head.coupled_count);
        let order: Vec<usize> = if head.mapping_family == 1 {
            wav_from_vorbis(nc).iter().copied().take(nc).collect()
        } else {
            (0..nc).collect()
        };
        let sources = order
            .iter()
            .map(|&vc| {
                let m = usize::from(head.mapping[vc]);
                if m == 255 {
                    None
                } else if m < 2 * coupled {
                    Some((m / 2, m & 1))
                } else {
                    Some((m - coupled, 0))
                }
            })
            .collect();
        let gain = if head.output_gain_db_q8 == 0 {
            1.0
        } else {
            10f32.powf(f32::from(head.output_gain_db_q8) / (20.0 * 256.0))
        };
        let scratch = streams
            .iter()
            .map(|s| vec![0.0; MAX_PACKET_SAMPLES * s.channels()])
            .collect();
        Ok(OpusDecoder {
            head,
            streams,
            gain,
            sources,
            scratch,
            last_samples: 0,
        })
    }

    /// A decoder from a container's codec configuration: empty → stereo (family 0); bytes
    /// starting with `OpusHead` → [`OpusHead::parse`]; anything else → [`OpusHead::from_dops`].
    pub fn from_config(config: &[u8]) -> Result<Self> {
        let head = if config.is_empty() {
            OpusHead::simple(2)
        } else if config.starts_with(b"OpusHead") {
            OpusHead::parse(config)?
        } else {
            OpusHead::from_dops(config)?
        };
        Self::new(&head)
    }

    /// The header the decoder was configured with.
    pub fn head(&self) -> &OpusHead {
        &self.head
    }

    /// Samples (per channel, 48 kHz) the caller should drop from the start of the stream.
    pub fn pre_skip(&self) -> u16 {
        self.head.pre_skip
    }

    /// The final range coder state of the first stream's last frame (for conformance tests).
    pub fn final_range(&self) -> u32 {
        self.streams
            .first()
            .map_or(0, stream::StreamDecoder::final_range)
    }

    fn decode_into(&mut self, packet: &[u8]) -> Result<usize> {
        let ns = self.streams.len();
        let plc = packet.is_empty();
        if !plc && packet.len() < 2 * ns - 1 {
            return Err(Error::invalid("opus: multistream packet too short"));
        }
        let plc_samples = if self.last_samples == 0 {
            960
        } else {
            self.last_samples
        };
        let mut offset = 0usize;
        let mut frame = 0usize;
        for s in 0..ns {
            let data = if plc { &[][..] } else { &packet[offset..] };
            if !plc && data.is_empty() {
                return Err(Error::invalid("opus: multistream packet truncated"));
            }
            let (n, used) =
                self.streams[s].decode(data, s + 1 != ns, &mut self.scratch[s], plc_samples)?;
            if s > 0 && n != frame {
                return Err(Error::invalid("opus: streams of different durations"));
            }
            frame = n;
            offset += used;
        }
        self.last_samples = frame;
        Ok(frame)
    }
}

impl Decoder for OpusDecoder {
    /// Decode one Opus packet (all streams). An empty packet conceals a lost packet of the
    /// previous packet's duration.
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer> {
        let n = self.decode_into(packet)?;
        let mut out = AudioBuffer::new(SAMPLE_RATE, self.sources.len(), n);
        for (dst, src) in out.channels.iter_mut().zip(&self.sources) {
            if let Some((s, c)) = *src {
                let sc = self.streams[s].channels();
                let buf = &self.scratch[s];
                for (i, d) in dst.iter_mut().enumerate() {
                    *d = buf[i * sc + c] * self.gain;
                }
            }
        }
        Ok(out)
    }

    fn reset(&mut self) {
        for s in &mut self.streams {
            s.reset();
        }
        self.last_samples = 0;
    }

    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn channels(&self) -> usize {
        usize::from(self.head.channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_head_round_trip() {
        let h = OpusHead::simple(6);
        assert_eq!(
            (h.mapping_family, h.stream_count, h.coupled_count),
            (1, 4, 2)
        );
        let b = h.to_bytes();
        assert_eq!(OpusHead::parse(&b).unwrap(), h);
        let h2 = OpusHead::simple(2);
        assert_eq!(OpusHead::parse(&h2.to_bytes()).unwrap(), h2);
        assert_eq!(OpusHead::simple(12).mapping_family, 255);
    }

    #[test]
    fn opus_head_errors() {
        assert!(OpusHead::parse(b"OpusHea").is_err());
        assert!(OpusHead::parse(b"OpusTags\x01\x02\x38\x01\x80\xbb\0\0\0\0\0").is_err());
        let mut b = OpusHead::simple(2).to_bytes();
        b[9] = 3; // 3 channels with family 0
        assert!(OpusHead::parse(&b).is_err());
        let mut b = OpusHead::simple(2).to_bytes();
        b[8] = 0x10; // version 16
        assert!(OpusHead::parse(&b).is_err());
        let mut b = OpusHead::simple(6).to_bytes();
        b.pop(); // truncated mapping
        assert!(OpusHead::parse(&b).is_err());
        let mut b = OpusHead::simple(6).to_bytes();
        *b.last_mut().unwrap() = 7; // index 7 >= 4 + 2
        assert!(OpusHead::parse(&b).is_err());
        let mut b = OpusHead::simple(6).to_bytes();
        b[20] = 1; // coupled 1... still fine; streams 0 is not
        b[19] = 0;
        assert!(OpusHead::parse(&b).is_err());
    }

    #[test]
    fn dops_parsing() {
        // Version 0, stereo, pre-skip 312, 48 kHz, gain -256 (-1 dB), family 0.
        let d = [0u8, 2, 0x01, 0x38, 0, 0, 0xbb, 0x80, 0xff, 0x00, 0];
        let h = OpusHead::from_dops(&d).unwrap();
        assert_eq!(h.channels, 2);
        assert_eq!(h.pre_skip, 312);
        assert_eq!(h.input_sample_rate, 48000);
        assert_eq!(h.output_gain_db_q8, -256);
        assert_eq!(h.mapping, vec![0, 1]);
        // Family 1, 6 channels.
        let mut d = vec![0u8, 6, 0x01, 0x38, 0, 0, 0xbb, 0x80, 0, 0, 1, 4, 2];
        d.extend_from_slice(&[0, 4, 1, 2, 3, 5]);
        let h = OpusHead::from_dops(&d).unwrap();
        assert_eq!((h.stream_count, h.coupled_count), (4, 2));
        assert!(OpusHead::from_dops(&d[..15]).is_err());
        assert!(OpusHead::from_dops(&[1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
        // from_config dispatch.
        assert_eq!(OpusDecoder::from_config(&[]).unwrap().channels(), 2);
        assert_eq!(OpusDecoder::from_config(&d).unwrap().channels(), 6);
        assert_eq!(
            OpusDecoder::from_config(&OpusHead::simple(1).to_bytes())
                .unwrap()
                .channels(),
            1
        );
    }

    #[test]
    fn wav_order_mapping() {
        // 5.1: Vorbis FL C FR RL RR LFE -> WAV FL FR C LFE BL BR.
        let d = OpusDecoder::new(&OpusHead::simple(6)).unwrap();
        // Mapping [0,4,1,2,3,5]: FL = stream 0 L, C = stream 2 (mono), FR = stream 0 R,
        // RL = stream 1 L, RR = stream 1 R, LFE = stream 3.
        assert_eq!(
            d.sources,
            vec![
                Some((0, 0)),
                Some((0, 1)),
                Some((2, 0)),
                Some((3, 0)),
                Some((1, 0)),
                Some((1, 1))
            ]
        );
    }
}

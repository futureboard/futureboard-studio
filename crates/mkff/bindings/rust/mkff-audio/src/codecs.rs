//! The codec registry: a decoder for any demuxed track, an encoder for any codec MKFF encodes.

use mkff_audio_core::{CodecId, Decoder, Encoder, Error, Result};
use mkff_audio_format::{PcmDecoder, PcmFormat, Track};

/// Every codec with a decoder.
pub const DECODABLE: [CodecId; 8] = [
    CodecId::Pcm,
    CodecId::MpegAudio,
    CodecId::Aac,
    CodecId::Flac,
    CodecId::Alac,
    CodecId::Ac3,
    CodecId::Opus,
    CodecId::Vorbis,
];

/// Every codec with an encoder.
pub const ENCODABLE: [CodecId; 5] = [
    CodecId::Pcm,
    CodecId::MpegAudio,
    CodecId::Aac,
    CodecId::Flac,
    CodecId::Alac,
];

/// A decoder for `codec` configured with `config` (the container's decoder configuration, see
/// [`mkff_audio_format::Track::config`]). PCM needs a [`Track`]; use [`decoder_for`].
pub fn decoder(codec: CodecId, config: &[u8]) -> Result<Box<dyn Decoder>> {
    Ok(match codec {
        CodecId::MpegAudio => Box::new(mkff_mp3::Mp3Decoder::new()),
        CodecId::Aac => Box::new(mkff_aac::AacDecoder::new(config)?),
        CodecId::Flac => Box::new(mkff_flac::FlacDecoder::new(config)?),
        CodecId::Alac => Box::new(mkff_alac::AlacDecoder::new(config)?),
        CodecId::Ac3 => Box::new(mkff_ac3::Ac3Decoder::new()),
        CodecId::Opus => Box::new(mkff_opus::OpusDecoder::from_config(config)?),
        CodecId::Vorbis => Box::new(mkff_vorbis::VorbisDecoder::new(config)?),
        CodecId::Pcm => {
            return Err(Error::InvalidArgument(
                "PCM needs its sample format: use decoder_for(track)".into(),
            ))
        }
    })
}

/// A decoder for a demuxed track.
pub fn decoder_for(track: &Track) -> Result<Box<dyn Decoder>> {
    match track.codec {
        CodecId::Pcm => Ok(Box::new(PcmDecoder::for_track(track)?)),
        other => decoder(other, &track.config),
    }
}

/// A decoder for raw PCM in `fmt`.
pub fn pcm_decoder(fmt: PcmFormat, channels: usize, sample_rate: u32) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(PcmDecoder::new(fmt, channels, sample_rate)?))
}

/// Encoder settings. Fields a codec doesn't use are ignored.
#[derive(Clone, Debug, PartialEq)]
pub struct EncoderSettings {
    pub codec: CodecId,
    pub sample_rate: u32,
    pub channels: usize,
    /// Bits per second (MP3, AAC). 0: a sensible default for the channel count.
    pub bitrate: u32,
    /// Bits per sample (FLAC, ALAC, PCM). 0: 16.
    pub bits_per_sample: u32,
    /// Speed / size trade-off: FLAC level 0–8, MP3 quality 0 (best) – 9.
    pub level: u8,
}

impl EncoderSettings {
    pub fn new(codec: CodecId, sample_rate: u32, channels: usize) -> Self {
        Self {
            codec,
            sample_rate,
            channels,
            bitrate: 0,
            bits_per_sample: 0,
            level: 5,
        }
    }

    pub fn with_bitrate(mut self, bps: u32) -> Self {
        self.bitrate = bps;
        self
    }

    pub fn with_bits(mut self, bits: u32) -> Self {
        self.bits_per_sample = bits;
        self
    }

    pub fn with_level(mut self, level: u8) -> Self {
        self.level = level;
        self
    }

    fn bitrate_or(&self, per_channel: u32, cap: u32) -> u32 {
        if self.bitrate > 0 {
            self.bitrate
        } else {
            (per_channel * self.channels.max(1) as u32).min(cap)
        }
    }

    fn bits(&self) -> u32 {
        if self.bits_per_sample == 0 {
            16
        } else {
            self.bits_per_sample
        }
    }
}

/// An encoder for `s.codec`. PCM "encoding" is little-endian integer (or 32-bit float when
/// `bits_per_sample` is 32) in packets of 4096 frames.
pub fn encoder(s: &EncoderSettings) -> Result<Box<dyn Encoder>> {
    Ok(match s.codec {
        CodecId::MpegAudio => {
            let mut c = mkff_mp3::Mp3EncoderConfig::new(
                s.sample_rate,
                s.channels,
                s.bitrate_or(64_000, 320_000) / 1000,
            );
            c.quality = s.level.min(9);
            Box::new(mkff_mp3::Mp3Encoder::new(c)?)
        }
        CodecId::Aac => {
            let c = mkff_aac::AacEncoderConfig::new(
                s.sample_rate,
                s.channels,
                s.bitrate_or(64_000, 640_000),
            );
            Box::new(mkff_aac::AacEncoder::new(c)?)
        }
        CodecId::Flac => {
            let c = mkff_flac::FlacEncoderConfig {
                sample_rate: s.sample_rate,
                channels: s.channels,
                bits_per_sample: s.bits(),
                level: s.level.min(8),
                ..Default::default()
            };
            Box::new(mkff_flac::FlacEncoder::new(c)?)
        }
        CodecId::Alac => {
            let c = mkff_alac::AlacEncoderConfig {
                sample_rate: s.sample_rate,
                channels: s.channels,
                bit_depth: s.bits(),
                ..Default::default()
            };
            Box::new(mkff_alac::AlacEncoder::new(c)?)
        }
        CodecId::Pcm => Box::new(PcmEncoder::new(s.sample_rate, s.channels, s.bits())?),
        other => return Err(Error::Unsupported(format!("no {} encoder", other.name()))),
    })
}

/// PCM "encoder": f32 in, little-endian integer / float packets out.
pub struct PcmEncoder {
    rate: u32,
    channels: usize,
    fmt: PcmFormat,
    pending: Vec<Vec<f32>>,
    pts: i64,
}

const PCM_PACKET: usize = 4096;

impl PcmEncoder {
    pub fn new(rate: u32, channels: usize, bits: u32) -> Result<Self> {
        let fmt = match bits {
            8 => PcmFormat::u8(),
            16 | 24 => PcmFormat::int(bits, false),
            32 => PcmFormat::float(32, false),
            b => return Err(Error::Unsupported(format!("{b}-bit PCM"))),
        };
        if channels == 0 {
            return Err(Error::InvalidArgument("0 channels".into()));
        }
        Ok(Self {
            rate,
            channels,
            fmt,
            pending: vec![Vec::new(); channels],
            pts: 0,
        })
    }

    pub fn format(&self) -> PcmFormat {
        self.fmt
    }

    fn take(&mut self, n: usize) -> Result<mkff_audio_core::Packet> {
        let buf = mkff_audio_core::AudioBuffer {
            sample_rate: self.rate,
            channels: self
                .pending
                .iter_mut()
                .map(|c| c.drain(..n).collect())
                .collect(),
        };
        let data = mkff_audio_format::encode_pcm(&buf, self.fmt)?;
        let p = mkff_audio_core::Packet {
            data,
            pts: self.pts,
            duration: n as u32,
        };
        self.pts += n as i64;
        Ok(p)
    }
}

impl Encoder for PcmEncoder {
    fn encode(&mut self, input: &[&[f32]]) -> Result<Vec<mkff_audio_core::Packet>> {
        let n = input.first().map_or(0, |c| c.len());
        for (c, p) in self.pending.iter_mut().enumerate() {
            match input.get(c) {
                Some(src) => p.extend_from_slice(&src[..n.min(src.len())]),
                None => p.extend(std::iter::repeat_n(0.0, n)),
            }
        }
        let mut out = Vec::new();
        while self.pending[0].len() >= PCM_PACKET {
            out.push(self.take(PCM_PACKET)?);
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<mkff_audio_core::Packet>> {
        let n = self.pending[0].len();
        Ok(if n > 0 {
            vec![self.take(n)?]
        } else {
            Vec::new()
        })
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn frame_size(&self) -> usize {
        0
    }

    fn priming(&self) -> u32 {
        0
    }

    fn config(&self) -> Vec<u8> {
        Vec::new()
    }
}

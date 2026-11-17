use mkff_audio::CodecId;

use crate::{Error, Result};

/// Audio codecs with a software decoder (MKFF's own, from `mkff-audio`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// AAC-LC and the AAC-LC core of HE-AAC v1 / v2 (upsampled to the SBR output rate).
    Aac,
    Opus,
    /// AC-3 and E-AC-3 (Dolby Digital / Dolby Digital Plus).
    Ac3,
    /// MPEG-1/2 audio layers I, II and III (MP3); the layer is read from the frame header.
    MpegAudio,
    Flac,
    Alac,
    Vorbis,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 7] = [
        AudioCodec::Aac,
        AudioCodec::Opus,
        AudioCodec::Ac3,
        AudioCodec::MpegAudio,
        AudioCodec::Flac,
        AudioCodec::Alac,
        AudioCodec::Vorbis,
    ];

    pub fn name(self) -> &'static str {
        match self {
            AudioCodec::Aac => "aac",
            AudioCodec::Opus => "opus",
            AudioCodec::Ac3 => "ac3",
            AudioCodec::MpegAudio => "mpeg-audio",
            AudioCodec::Flac => "flac",
            AudioCodec::Alac => "alac",
            AudioCodec::Vorbis => "vorbis",
        }
    }

    /// Parse a codec name (`aac`, `opus`, `ac3`/`eac3`, `mp1`/`mp2`/`mp3`, `flac`, `alac`,
    /// `vorbis`), case-insensitively.
    pub fn from_name(s: &str) -> Option<AudioCodec> {
        Some(match s.to_ascii_lowercase().as_str() {
            "aac" | "mp4a" => AudioCodec::Aac,
            "opus" => AudioCodec::Opus,
            "ac3" | "ac-3" | "eac3" | "e-ac-3" | "ec-3" => AudioCodec::Ac3,
            "mp1" | "mp2" | "mp3" | "mpa" | "mpeg-audio" => AudioCodec::MpegAudio,
            "flac" => AudioCodec::Flac,
            "alac" => AudioCodec::Alac,
            "vorbis" => AudioCodec::Vorbis,
            _ => return None,
        })
    }

    /// The `mkff-audio` codec id.
    pub fn id(self) -> CodecId {
        match self {
            AudioCodec::Aac => CodecId::Aac,
            AudioCodec::Opus => CodecId::Opus,
            AudioCodec::Ac3 => CodecId::Ac3,
            AudioCodec::MpegAudio => CodecId::MpegAudio,
            AudioCodec::Flac => CodecId::Flac,
            AudioCodec::Alac => CodecId::Alac,
            AudioCodec::Vorbis => CodecId::Vorbis,
        }
    }
}

/// Decoded audio: planar f32, one `Vec` per channel, all the same length. Surround layouts are in
/// WAV / SMPTE order (L R C LFE Ls Rs …).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
}

impl AudioBuffer {
    /// Samples per channel.
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Interleaved samples (L R L R … for stereo).
    pub fn interleaved(&self) -> Vec<f32> {
        let (n, ch) = (self.frames(), self.channels.len());
        let mut out = Vec::with_capacity(n * ch);
        for i in 0..n {
            for c in &self.channels {
                out.push(c.get(i).copied().unwrap_or(0.0));
            }
        }
        out
    }
}

impl From<mkff_audio::AudioBuffer> for AudioBuffer {
    fn from(b: mkff_audio::AudioBuffer) -> Self {
        AudioBuffer { sample_rate: b.sample_rate, channels: b.channels }
    }
}

/// A stateful software audio decoder for one elementary stream.
///
/// | Codec | Packet | Configuration |
/// |-------|--------|---------------|
/// | AAC | Raw access unit, or an ADTS / LOAS frame when no configuration is given | `AudioSpecificConfig` (`esds` / Matroska `CodecPrivate`) |
/// | Opus | One Opus packet | `OpusHead` (Ogg / Matroska) or `dOps` payload; empty: stereo, no pre-skip |
/// | AC-3 / E-AC-3 | One or more syncframes | — |
/// | MPEG audio | One or more frames | — |
/// | FLAC | One FLAC frame | `STREAMINFO` (with or without the `fLaC` marker / block header) |
/// | ALAC | One packet | ALAC magic cookie |
/// | Vorbis | One packet | Identification + setup headers, or Matroska Xiph-laced `CodecPrivate` |
pub struct AudioDecoder {
    inner: Box<dyn mkff_audio::Decoder>,
    codec: AudioCodec,
    /// The container's rate, until the decoder knows its own.
    sample_rate: u32,
}

impl AudioDecoder {
    /// `sample_rate`: the stream's rate from the container (0 when unknown: it is then read from
    /// the configuration or the first frame header).
    pub fn new(codec: AudioCodec, sample_rate: u32, config: &[u8]) -> Result<Self> {
        let inner = mkff_audio::decoder(codec.id(), config)?;
        Ok(Self { inner, codec, sample_rate })
    }

    pub fn codec(&self) -> AudioCodec {
        self.codec
    }

    /// The output sample rate (0 until known: for header-configured codecs, after the first
    /// packet).
    pub fn sample_rate(&self) -> u32 {
        match self.inner.sample_rate() {
            0 => self.sample_rate,
            r => r,
        }
    }

    /// Decode one packet. `ts` is the packet's timestamp (unused: MKFF's decoders are driven
    /// by the packets alone).
    pub fn decode(&mut self, packet: &[u8], _ts: u64) -> Result<AudioBuffer> {
        let mut b: AudioBuffer = self.inner.decode(packet)?.into();
        if b.sample_rate == 0 {
            b.sample_rate = self.sample_rate();
        }
        Ok(b)
    }

    /// Forget decoding state (after a seek).
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

impl std::fmt::Debug for AudioDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioDecoder")
            .field("codec", &self.codec)
            .field("sample_rate", &self.sample_rate())
            .finish()
    }
}

impl From<mkff_audio::Error> for Error {
    fn from(e: mkff_audio::Error) -> Self {
        match e {
            mkff_audio::Error::Unsupported(s) => Error::Unsupported(s),
            mkff_audio::Error::InvalidData(s) => Error::Decode(s),
            mkff_audio::Error::EndOfData => Error::Decode("truncated data".into()),
            mkff_audio::Error::InvalidArgument(s) => Error::InvalidArgument(s),
            mkff_audio::Error::Io(s) => Error::Io(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_constructs() {
        for c in AudioCodec::ALL {
            // header-configured codecs take an empty configuration
            let empty_ok = !matches!(c, AudioCodec::Alac | AudioCodec::Flac);
            if empty_ok {
                AudioDecoder::new(c, 48_000, &[]).unwrap_or_else(|e| panic!("{c:?}: {e}"));
            }
        }
    }
}

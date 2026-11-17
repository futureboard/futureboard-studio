//! Media formats and format negotiation (BeOS `media_format`).
//!
//! A [`MediaFormat`] describes what flows through a connection or what a codec add-on accepts.
//! Any field may be left as a wildcard (`0`, `None`, or [`SampleFormat::Any`] /
//! [`PixelFormat::Any`]); [`MediaFormat::matches`] treats wildcards as "anything", and
//! [`MediaFormat::specialize`] fills one format's wildcards from another — the two steps
//! `BMediaRoster::Connect` performs between a producer's proposal and a consumer's acceptance.

use std::fmt;

/// Codec families the Media Kit routes between add-ons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CodecFamily {
    // audio
    Pcm,
    MpegAudio,
    Aac,
    Flac,
    Alac,
    Ac3,
    Opus,
    Vorbis,
    // video
    H264,
    Hevc,
    Vp8,
    Vp9,
    Av1,
    Mpeg2,
    ProRes,
    Dnx,
    Apv,
    Mjpeg,
}

impl CodecFamily {
    pub fn name(self) -> &'static str {
        match self {
            CodecFamily::Pcm => "pcm",
            CodecFamily::MpegAudio => "mpeg-audio",
            CodecFamily::Aac => "aac",
            CodecFamily::Flac => "flac",
            CodecFamily::Alac => "alac",
            CodecFamily::Ac3 => "ac3",
            CodecFamily::Opus => "opus",
            CodecFamily::Vorbis => "vorbis",
            CodecFamily::H264 => "h264",
            CodecFamily::Hevc => "hevc",
            CodecFamily::Vp8 => "vp8",
            CodecFamily::Vp9 => "vp9",
            CodecFamily::Av1 => "av1",
            CodecFamily::Mpeg2 => "mpeg2",
            CodecFamily::ProRes => "prores",
            CodecFamily::Dnx => "dnxhd",
            CodecFamily::Apv => "apv",
            CodecFamily::Mjpeg => "mjpeg",
        }
    }

    pub fn is_video(self) -> bool {
        self >= CodecFamily::H264
    }

    pub const AUDIO: [CodecFamily; 8] = [
        CodecFamily::Pcm,
        CodecFamily::MpegAudio,
        CodecFamily::Aac,
        CodecFamily::Flac,
        CodecFamily::Alac,
        CodecFamily::Ac3,
        CodecFamily::Opus,
        CodecFamily::Vorbis,
    ];

    pub const VIDEO: [CodecFamily; 10] = [
        CodecFamily::H264,
        CodecFamily::Hevc,
        CodecFamily::Vp8,
        CodecFamily::Vp9,
        CodecFamily::Av1,
        CodecFamily::Mpeg2,
        CodecFamily::ProRes,
        CodecFamily::Dnx,
        CodecFamily::Apv,
        CodecFamily::Mjpeg,
    ];
}

impl fmt::Display for CodecFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Raw audio sample layout (`media_raw_audio_format::format`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    #[default]
    Any,
    /// 32-bit float, planar (MKFF's native layout).
    F32Planar,
    /// 32-bit float, interleaved (BeOS `B_AUDIO_FLOAT`).
    F32,
    /// 16-bit signed, interleaved (`B_AUDIO_SHORT`).
    S16,
    /// 32-bit signed, interleaved (`B_AUDIO_INT`).
    S32,
    /// 8-bit unsigned, interleaved (`B_AUDIO_UCHAR`).
    U8,
}

impl SampleFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::Any => 0,
            SampleFormat::F32Planar | SampleFormat::F32 | SampleFormat::S32 => 4,
            SampleFormat::S16 => 2,
            SampleFormat::U8 => 1,
        }
    }
}

/// Raw picture layout (`color_space`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    #[default]
    Any,
    /// 8-bit 4:2:0, Y plane + interleaved CbCr plane.
    Nv12,
    /// 10-bit 4:2:0 in 16-bit words (MSB-aligned), Y + interleaved CbCr.
    P010,
    /// 8-bit 4:2:0, three planes.
    I420,
    /// 8-bit RGBA, straight alpha.
    Rgba8,
    /// 8-bit BGRA (`B_RGB32`), the usual swap-chain format.
    Bgra8,
}

/// Uncompressed audio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RawAudioFormat {
    /// Hz (0: any).
    pub frame_rate: u32,
    /// 0: any.
    pub channels: u32,
    pub format: SampleFormat,
    /// Frames per buffer (0: any).
    pub buffer_frames: u32,
}

/// Uncompressed video.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RawVideoFormat {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Frames per second as a fraction (0/0: any).
    pub fps: (u32, u32),
    /// Frames can live in GPU memory ([`crate::buffer::Payload::Gpu`]).
    pub gpu: bool,
}

/// Compressed audio or video.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EncodedFormat {
    pub codec: CodecFamily,
    /// Decoder configuration as a container stores it (AudioSpecificConfig, avcC, …); empty when
    /// configured in-band.
    pub config: Vec<u8>,
    /// Audio: sample rate / channels; video: width / height. 0: unknown or any.
    pub a: u32,
    pub b: u32,
    /// Bits per second (0: unknown).
    pub bit_rate: u32,
}

/// What a connection or codec carries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum MediaFormat {
    /// Matches anything (`B_MEDIA_UNKNOWN_TYPE` with wildcards).
    #[default]
    Wildcard,
    RawAudio(RawAudioFormat),
    RawVideo(RawVideoFormat),
    EncodedAudio(EncodedFormat),
    EncodedVideo(EncodedFormat),
}

impl MediaFormat {
    pub fn raw_audio(frame_rate: u32, channels: u32, format: SampleFormat) -> Self {
        MediaFormat::RawAudio(RawAudioFormat {
            frame_rate,
            channels,
            format,
            buffer_frames: 0,
        })
    }

    pub fn raw_video(width: u32, height: u32, format: PixelFormat) -> Self {
        MediaFormat::RawVideo(RawVideoFormat {
            width,
            height,
            format,
            fps: (0, 0),
            gpu: false,
        })
    }

    pub fn encoded(codec: CodecFamily, config: Vec<u8>, a: u32, b: u32) -> Self {
        let e = EncodedFormat {
            codec,
            config,
            a,
            b,
            bit_rate: 0,
        };
        if codec.is_video() {
            MediaFormat::EncodedVideo(e)
        } else {
            MediaFormat::EncodedAudio(e)
        }
    }

    /// The codec family of an encoded format (or `Pcm` for raw audio).
    pub fn codec(&self) -> Option<CodecFamily> {
        match self {
            MediaFormat::EncodedAudio(e) | MediaFormat::EncodedVideo(e) => Some(e.codec),
            MediaFormat::RawAudio(_) => Some(CodecFamily::Pcm),
            _ => None,
        }
    }

    pub fn is_audio(&self) -> bool {
        matches!(
            self,
            MediaFormat::RawAudio(_) | MediaFormat::EncodedAudio(_)
        )
    }

    pub fn is_video(&self) -> bool {
        matches!(
            self,
            MediaFormat::RawVideo(_) | MediaFormat::EncodedVideo(_)
        )
    }

    /// Whether the two formats are compatible, treating unset fields on either side as wildcards
    /// (`format_is_compatible`).
    pub fn matches(&self, other: &MediaFormat) -> bool {
        fn n(a: u32, b: u32) -> bool {
            a == 0 || b == 0 || a == b
        }
        match (self, other) {
            (MediaFormat::Wildcard, _) | (_, MediaFormat::Wildcard) => true,
            (MediaFormat::RawAudio(a), MediaFormat::RawAudio(b)) => {
                n(a.frame_rate, b.frame_rate)
                    && n(a.channels, b.channels)
                    && n(a.buffer_frames, b.buffer_frames)
                    && (a.format == SampleFormat::Any
                        || b.format == SampleFormat::Any
                        || a.format == b.format)
            }
            (MediaFormat::RawVideo(a), MediaFormat::RawVideo(b)) => {
                n(a.width, b.width)
                    && n(a.height, b.height)
                    && (a.format == PixelFormat::Any
                        || b.format == PixelFormat::Any
                        || a.format == b.format)
                    && (a.fps == (0, 0) || b.fps == (0, 0) || a.fps == b.fps)
            }
            (MediaFormat::EncodedAudio(a), MediaFormat::EncodedAudio(b))
            | (MediaFormat::EncodedVideo(a), MediaFormat::EncodedVideo(b)) => {
                a.codec == b.codec && n(a.a, b.a) && n(a.b, b.b)
            }
            _ => false,
        }
    }

    /// Fill this format's wildcards from `other` (which should [`match`](Self::matches)).
    pub fn specialize(&mut self, other: &MediaFormat) {
        fn f(a: &mut u32, b: u32) {
            if *a == 0 {
                *a = b;
            }
        }
        match (self, other) {
            (s @ MediaFormat::Wildcard, o) => *s = o.clone(),
            (MediaFormat::RawAudio(a), MediaFormat::RawAudio(b)) => {
                f(&mut a.frame_rate, b.frame_rate);
                f(&mut a.channels, b.channels);
                f(&mut a.buffer_frames, b.buffer_frames);
                if a.format == SampleFormat::Any {
                    a.format = b.format;
                }
            }
            (MediaFormat::RawVideo(a), MediaFormat::RawVideo(b)) => {
                f(&mut a.width, b.width);
                f(&mut a.height, b.height);
                if a.format == PixelFormat::Any {
                    a.format = b.format;
                }
                if a.fps == (0, 0) {
                    a.fps = b.fps;
                }
                a.gpu &= b.gpu;
            }
            (MediaFormat::EncodedAudio(a), MediaFormat::EncodedAudio(b))
            | (MediaFormat::EncodedVideo(a), MediaFormat::EncodedVideo(b)) => {
                f(&mut a.a, b.a);
                f(&mut a.b, b.b);
                f(&mut a.bit_rate, b.bit_rate);
                if a.config.is_empty() {
                    a.config = b.config.clone();
                }
            }
            _ => {}
        }
    }

    /// No wildcard left (ready to carry buffers).
    pub fn is_concrete(&self) -> bool {
        match self {
            MediaFormat::Wildcard => false,
            MediaFormat::RawAudio(a) => {
                a.frame_rate != 0 && a.channels != 0 && a.format != SampleFormat::Any
            }
            MediaFormat::RawVideo(v) => {
                v.width != 0 && v.height != 0 && v.format != PixelFormat::Any
            }
            MediaFormat::EncodedAudio(_) | MediaFormat::EncodedVideo(_) => true,
        }
    }
}

impl fmt::Display for MediaFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaFormat::Wildcard => write!(f, "*"),
            MediaFormat::RawAudio(a) => write!(
                f,
                "raw audio {} Hz × {} ({:?})",
                a.frame_rate, a.channels, a.format
            ),
            MediaFormat::RawVideo(v) => {
                write!(
                    f,
                    "raw video {}×{} {:?}{}",
                    v.width,
                    v.height,
                    v.format,
                    if v.gpu { " (GPU)" } else { "" }
                )
            }
            MediaFormat::EncodedAudio(e) => write!(f, "{} audio {} Hz × {}", e.codec, e.a, e.b),
            MediaFormat::EncodedVideo(e) => write!(f, "{} video {}×{}", e.codec, e.a, e.b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        let proposal = MediaFormat::raw_audio(48000, 2, SampleFormat::Any);
        let mut accepted = MediaFormat::raw_audio(0, 0, SampleFormat::F32);
        assert!(proposal.matches(&accepted));
        accepted.specialize(&proposal);
        assert_eq!(
            accepted,
            MediaFormat::raw_audio(48000, 2, SampleFormat::F32)
        );
        assert!(accepted.is_concrete());
        assert!(!MediaFormat::raw_audio(44100, 2, SampleFormat::F32).matches(&accepted));
        let aac = MediaFormat::encoded(CodecFamily::Aac, vec![], 0, 0);
        assert!(aac.matches(&MediaFormat::encoded(
            CodecFamily::Aac,
            vec![0x12, 0x10],
            44100,
            2
        )));
        assert!(!aac.matches(&MediaFormat::encoded(CodecFamily::Opus, vec![], 0, 0)));
        assert!(MediaFormat::encoded(CodecFamily::H264, vec![], 0, 0).is_video());
    }
}

use filmcraft_codecs::video as fv;

use crate::{Error, Frame, Result};

/// Video codecs with a pure-Rust decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// H.264 / AVC (8-bit 4:2:0: Baseline, Main, High, CAVLC and CABAC).
    H264,
    /// H.265 / HEVC Main and Main 10.
    Hevc,
    Vp8,
    /// VP9 profiles 0-3 (8 / 10 / 12-bit, 4:2:0 / 4:2:2 / 4:4:4).
    Vp9,
    Av1,
    Mpeg1,
    /// MPEG-2 Main and 4:2:2 profile (frame and field pictures).
    Mpeg2,
    /// Apple ProRes 422 (Proxy / LT / standard / HQ) and 4444 (XQ), with alpha.
    ProRes,
    /// Avid DNxHD / DNxHR (SMPTE VC-3).
    Dnx,
    /// Advanced Professional Video (RFC 9924).
    Apv,
    /// Motion JPEG / Photo JPEG.
    Mjpeg,
}

impl VideoCodec {
    pub const ALL: [VideoCodec; 11] = [
        VideoCodec::H264,
        VideoCodec::Hevc,
        VideoCodec::Vp8,
        VideoCodec::Vp9,
        VideoCodec::Av1,
        VideoCodec::Mpeg1,
        VideoCodec::Mpeg2,
        VideoCodec::ProRes,
        VideoCodec::Dnx,
        VideoCodec::Apv,
        VideoCodec::Mjpeg,
    ];

    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
            VideoCodec::Vp8 => "vp8",
            VideoCodec::Vp9 => "vp9",
            VideoCodec::Av1 => "av1",
            VideoCodec::Mpeg1 => "mpeg1",
            VideoCodec::Mpeg2 => "mpeg2",
            VideoCodec::ProRes => "prores",
            VideoCodec::Dnx => "dnxhd",
            VideoCodec::Apv => "apv",
            VideoCodec::Mjpeg => "mjpeg",
        }
    }

    /// Parse a codec name (`h264`/`avc`, `hevc`/`h265`, `vp8`, `vp9`, `av1`, `mpeg1`, `mpeg2`,
    /// `prores`, `dnxhd`/`dnxhr`/`vc3`, `apv`, `mjpeg`/`jpeg`), case-insensitively.
    pub fn from_name(s: &str) -> Option<VideoCodec> {
        Some(match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" | "avc1" | "h.264" => VideoCodec::H264,
            "hevc" | "h265" | "h.265" | "hvc1" | "hev1" => VideoCodec::Hevc,
            "vp8" => VideoCodec::Vp8,
            "vp9" => VideoCodec::Vp9,
            "av1" => VideoCodec::Av1,
            "mpeg1" | "mpeg1video" | "m1v" => VideoCodec::Mpeg1,
            "mpeg2" | "mpeg2video" | "m2v" => VideoCodec::Mpeg2,
            "prores" => VideoCodec::ProRes,
            "dnxhd" | "dnxhr" | "dnx" | "vc3" | "vc-3" => VideoCodec::Dnx,
            "apv" => VideoCodec::Apv,
            "mjpeg" | "jpeg" | "mjpg" => VideoCodec::Mjpeg,
            _ => return None,
        })
    }
}

/// A stateful software video decoder for one elementary stream.
///
/// Packets go in in decode order; frames come out in presentation order, so a call may return
/// none (the decoder is holding pictures for reordering) or several. Call [`Self::flush`] at the
/// end of the stream for the rest.
///
/// Packet formats:
///
/// | Codec | Packet | Configuration ([`Self::with_config`]) |
/// |-------|--------|---------------------------------------|
/// | H.264 / HEVC | Annex-B access unit (start codes, in-band SPS/PPS) or, with an `avcC` / `hvcC` configuration, length-prefixed NAL units as stored in MP4 / Matroska | `avcC` / `hvcC` record, or Annex-B parameter sets |
/// | VP8 / VP9 | One frame (IVF / WebM block payload) | — |
/// | AV1 | One temporal unit of low-overhead OBUs | `av1C` record or the sequence header OBU |
/// | MPEG-1 / 2 | One picture (with any preceding sequence / GOP headers) | Sequence header, for streams that open on a picture without one |
/// | ProRes / DNx / APV / MJPEG | One frame | — |
pub struct VideoDecoder {
    inner: Box<dyn fv::VideoDecoder>,
    codec: VideoCodec,
    /// In-band configuration fed in front of the first packet after creation or a reset
    /// (Annex-B parameter sets).
    prefix: Vec<u8>,
    primed: bool,
}

fn is_annexb(b: &[u8]) -> bool {
    b.starts_with(&[0, 0, 1]) || b.starts_with(&[0, 0, 0, 1])
}

impl VideoDecoder {
    /// A decoder whose configuration arrives in-band (Annex-B parameter sets, sequence headers,
    /// keyframe headers).
    pub fn new(codec: VideoCodec) -> Result<Self> {
        Self::with_config(codec, &[])
    }

    /// A decoder configured from the container's codec configuration record (see the table on
    /// [`VideoDecoder`]). An empty `config` is the same as [`Self::new`].
    pub fn with_config(codec: VideoCodec, config: &[u8]) -> Result<Self> {
        let mut prefix = Vec::new();
        let inner: Box<dyn fv::VideoDecoder> = match codec {
            VideoCodec::H264 if config.is_empty() || is_annexb(config) => {
                prefix = config.to_vec();
                Box::new(fv::H264Decoder::annexb())
            }
            VideoCodec::H264 => Box::new(fv::H264Decoder::new(config.to_vec())?),
            VideoCodec::Hevc if config.is_empty() || is_annexb(config) => {
                prefix = config.to_vec();
                Box::new(fv::HevcDecoder::annexb())
            }
            VideoCodec::Hevc => Box::new(fv::HevcDecoder::new(config.to_vec())?),
            VideoCodec::Vp8 => Box::new(fv::Vp8Decoder::new()),
            VideoCodec::Vp9 => Box::new(fv::Vp9Decoder::new(None)),
            VideoCodec::Av1 => {
                // av1C: marker + version byte 0x81, three bytes of fields, then the config OBUs
                let obus = if config.len() >= 4 && config[0] == 0x81 {
                    &config[4..]
                } else {
                    config
                };
                Box::new(fv::Av1Decoder::new(obus.to_vec()))
            }
            VideoCodec::Mpeg1 | VideoCodec::Mpeg2 => {
                let header = if config.is_empty() {
                    Vec::new()
                } else {
                    fv::mpeg2_sequence_header(config).unwrap_or_else(|| config.to_vec())
                };
                Box::new(fv::Mpeg2Decoder::new(header))
            }
            VideoCodec::ProRes => Box::new(fv::ProResDecoder),
            VideoCodec::Dnx => Box::new(fv::DnxDecoder),
            VideoCodec::Apv => Box::new(fv::ApvDecoder),
            VideoCodec::Mjpeg => Box::new(fv::MjpegDecoder),
        };
        Ok(Self {
            inner,
            codec,
            prefix,
            primed: false,
        })
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The decoder implementation's name (e.g. "FilmCraft HEVC").
    pub fn name(&self) -> &str {
        self.inner.name()
    }

    /// Every picture is coded independently (ProRes, DNx, APV, MJPEG): no reordering, frames can
    /// be decoded in any order.
    pub fn intra_only(&self) -> bool {
        self.inner.intra_only()
    }

    /// Decode one packet. `pts` is echoed on the frame it produces.
    pub fn decode(&mut self, packet: &[u8], pts: i64) -> Result<Vec<Frame>> {
        let frames = if self.primed || self.prefix.is_empty() {
            self.inner.decode(packet, pts)?
        } else {
            let mut joined = self.prefix.clone();
            joined.extend_from_slice(packet);
            self.inner.decode(&joined, pts)?
        };
        self.primed = true;
        Ok(frames
            .into_iter()
            .map(|d| Frame::new(d.frame, d.pts))
            .collect())
    }

    /// Drain the pictures held for reordering (end of stream, or before a seek).
    pub fn flush(&mut self) -> Vec<Frame> {
        self.inner
            .flush()
            .into_iter()
            .map(|d| Frame::new(d.frame, d.pts))
            .collect()
    }

    /// Forget all decoding state (after seeking to a random-access point).
    pub fn reset(&mut self) {
        self.inner.reset();
        self.primed = false;
    }

    /// Whether decoding can start at `packet` (`None`: the codec cannot tell from the packet
    /// alone; trust the container's sync flag).
    pub fn is_random_access(&self, packet: &[u8]) -> Option<bool> {
        if self.intra_only() {
            return Some(true);
        }
        self.inner.is_random_access(packet)
    }
}

impl std::fmt::Debug for VideoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoDecoder")
            .field("codec", &self.codec)
            .field("name", &self.inner.name())
            .finish()
    }
}

impl From<VideoCodec> for &'static str {
    fn from(c: VideoCodec) -> Self {
        c.name()
    }
}

impl std::str::FromStr for VideoCodec {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        VideoCodec::from_name(s).ok_or_else(|| Error::Unsupported(format!("video codec {s:?}")))
    }
}

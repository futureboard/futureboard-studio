//! Pure-Rust software demux and decode for MKFF: every codec, no FFmpeg.
//!
//! `libmkff` decodes H.264 and HEVC on the platform's video hardware (and `mkff-mediakit`'s
//! accelerants do the same through D3D11 / DXVA on Windows). This crate covers everything in
//! software, in safe Rust, on any platform:
//!
//! - **Audio** is MKFF's own: the `mkff-audio` stack (`mkff-mp3`, `mkff-aac`, `mkff-flac`,
//!   `mkff-alac`, `mkff-ac3`, `mkff-opus`, `mkff-vorbis`, `mkff-audio-format`), written from the
//!   specifications. WAV / AIFF, MP3, ADTS, FLAC, Ogg and MP4 / MOV audio never go through
//!   FilmCraft.
//! - **Video**, and the audio of Matroska / WebM, AVI, MXF and MPEG-TS / PS (which MKFF does not
//!   demux itself yet), still come from the FilmCraft codec crates
//!   (<https://github.com/storytold/filmcraft>, MIT OR Apache-2.0), being replaced step by step.
//!
//! | Kind | Formats |
//! |------|---------|
//! | Containers | MP4 / MOV / 3GP, Matroska / WebM, AVI (OpenDML), MXF (OP1a / OP-Atom), MPEG-TS / M2TS, MPEG-PS / VOB, Ogg, IVF, YUV4MPEG2, raw elementary streams |
//! | Video | H.264 (8-bit 4:2:0), HEVC (Main / Main 10), VP8, VP9 (profiles 0-3), AV1, MPEG-1 / MPEG-2, ProRes (422 / 4444), DNxHD / DNxHR (VC-3), APV, Motion JPEG, uncompressed |
//! | Audio | AAC (LC, HE-AAC core, ADTS / LATM), Opus, AC-3 / E-AC-3, MPEG audio layers I / II / III, FLAC, ALAC, Vorbis, PCM / LPCM |
//!
//! Two levels of API:
//!
//! - [`MediaFile`]: open any supported file, read its [`MediaInfo`], get the [`Frame`] at a frame
//!   index or time (GOP-aware seeking: decode from the preceding sync sample), and read audio as
//!   planar f32 at any sample rate.
//! - [`VideoDecoder`] / [`AudioDecoder`]: stateful packet decoders for elementary streams, e.g.
//!   the Annex-B access units from `mkff::Mp4Demux`, or [`ivf::IvfReader`] frames.
//!
//! [`encode`]: H.264 / ProRes video (FilmCraft) and AAC-LC audio (`mkff-aac`) encoders and an
//! MP4 / MOV writer
//! (progressive, or fragmented for crash-safe live recording).
//!
//! Frames come out as planar Y'CbCr (8 or 16-bit, 4:2:0 / 4:2:2 / 4:4:4, optional alpha) or RGBA;
//! [`Frame::to_nv12`], [`Frame::to_p010`] and [`Frame::to_rgba8`] convert to the formats the rest
//! of MKFF (and a GPU upload) expects.

mod audio;
pub mod encode;
mod error;
mod frame;
pub mod ivf;
mod media;
mod video;

pub use audio::{AudioBuffer, AudioCodec, AudioDecoder};
pub use error::{Error, Result};
pub use frame::{ChromaFormat, ColorDesc, Frame, Matrix, PixelLayout, Planes};
pub use media::{AudioStreamInfo, MediaFile, MediaInfo, MediaKind, VideoStreamInfo};
pub use video::{VideoCodec, VideoDecoder};

/// The FilmCraft crates this one adapts, for callers that need the full engine-level API.
pub mod filmcraft {
    pub use filmcraft_codecs as codecs;
    pub use filmcraft_frame as frame;
    pub use filmcraft_media as media;
}

/// File extensions [`MediaFile::open`] recognises (video and audio; stills are opened too).
pub fn supported_extensions() -> impl Iterator<Item = &'static str> {
    let audio = mkff_audio::supported_extensions();
    filmcraft_media::VIDEO_EXTENSIONS
        .iter()
        .chain(audio.iter())
        .chain(filmcraft_media::AUDIO_EXTENSIONS.iter().filter(move |e| !audio.contains(e)))
        .copied()
}

//! Safe RAII wrapper around libmkff. Hardware frames stay GPU-native;
//! CPU planes (software or HW readback) via [`VideoFrame::map_cpu_planes`].
//!
//! With the `software-codecs` feature, [`codecs`] adds pure-Rust demux and
//! decode of every other container and codec (VP8/VP9/AV1, MPEG-2, ProRes,
//! DNxHD/HR, APV, MJPEG, software H.264 / HEVC Main10, and AAC / Opus / AC-3 /
//! MP3 / FLAC / Vorbis audio) — no FFmpeg.

mod context;
mod decoder;
mod error;
mod frame;
#[cfg(target_os = "linux")]
mod linux;
mod mp4_demux;

pub use context::Context;
pub use decoder::{DecoderInfo, ReceiveOutcome, VideoDecoder};
pub use error::{MkffError, Result};
pub use frame::{CpuPlanes, FrameInfo, VideoFrame};
#[cfg(target_os = "linux")]
pub use frame::{LinuxDmaBuf, LinuxDmaBufObject, LinuxDmaBufPlane};
#[cfg(target_os = "linux")]
pub use linux::{DrmDeviceInfo, VaInfo, VaProfileInfo};
#[cfg(feature = "software-codecs")]
pub use mkff_codecs as codecs;
pub use mp4_demux::{Mp4AccessUnit, Mp4Demux, Mp4VideoTrackInfo, ReadAuOutcome};

pub use mkff_sys::{
    MKFF_PixelFormat as PixelFormat, MKFF_VideoBackend as VideoBackend, MKFF_VideoCodec as VideoCodec,
    MKFF_VideoEntrypoint as VideoEntrypoint, MKFF_VideoProfile as VideoProfile,
};

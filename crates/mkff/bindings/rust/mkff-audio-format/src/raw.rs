//! Headerless (raw) interleaved PCM reading.

use std::path::Path;

use mkff_audio_core::{CodecId, Error, Result};

use crate::io::Source;
use crate::pcm::PcmFormat;
use crate::wav::PcmRegion;
use crate::{Demuxer, ReadSeek, Track};

/// Extensions Futureboard treats as headerless PCM (`"raw"`, `"pcm"`); NOT in
/// [`AUDIO_EXTENSIONS`](crate::AUDIO_EXTENSIONS).
pub const RAW_PCM_EXTENSIONS: &[&str] = &["raw", "pcm"];

/// Layout of a headerless PCM file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawPcmSpec {
    /// Sample rate in Hz (`> 0`).
    pub sample_rate: u32,
    /// Interleaved channel count (`1..=64`).
    pub channels: u32,
    /// Sample coding and byte order.
    pub format: PcmFormat,
    /// Bytes to skip before the first sample (0 for a pure raw dump).
    pub data_offset: u64,
}

fn validate_spec(spec: &RawPcmSpec) -> Result<()> {
    if spec.sample_rate == 0 {
        return Err(Error::InvalidArgument(
            "RAW PCM sample_rate must be non-zero".into(),
        ));
    }
    if !(1..=64).contains(&spec.channels) {
        return Err(Error::InvalidArgument(
            "RAW PCM channels must be in 1..=64".into(),
        ));
    }
    if !spec.format.is_valid() {
        return Err(Error::InvalidArgument(format!(
            "unsupported RAW PCM format {}",
            spec.format.label()
        )));
    }
    Ok(())
}

/// Open a headerless PCM byte stream with an explicit [`RawPcmSpec`].
pub fn open_raw_pcm(reader: Box<dyn ReadSeek>, spec: RawPcmSpec) -> Result<Box<dyn Demuxer>> {
    validate_spec(&spec)?;
    let src = Source::new(reader)?;
    let mut track = Track::new(
        CodecId::Pcm,
        spec.sample_rate,
        spec.channels,
        format!("PCM {}", spec.format.label()),
    );
    track.pcm = Some(spec.format);
    track.bits_per_sample = Some(spec.format.bits);
    let avail = src.len().saturating_sub(spec.data_offset);
    Ok(Box::new(PcmRegion::new(
        src,
        "RAW PCM",
        track,
        spec.data_offset,
        avail,
    )?))
}

/// Open a headerless PCM file at `path` with an explicit [`RawPcmSpec`].
pub fn open_raw_pcm_path(path: impl AsRef<Path>, spec: RawPcmSpec) -> Result<Box<dyn Demuxer>> {
    validate_spec(&spec)?;
    let file = std::fs::File::open(path.as_ref())?;
    open_raw_pcm(Box::new(file), spec)
}

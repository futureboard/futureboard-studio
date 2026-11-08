//! Format-agnostic encoding surface for SphereEncoder.
//!
//! SphereEncoder is responsible only for turning interleaved PCM frames into a
//! valid container on disk. It never renders audio and never spawns external
//! processes — in particular there is no FFmpeg/libav anywhere in this crate.
//! Arrangement rendering lives in the audio engine; building an export request
//! from project state lives in the UI/layout layer.

use std::path::Path;

use crate::metadata::AudioMetadata;

/// Container/codec a render is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioFileFormat {
    #[default]
    Wav,
    Flac,
    Mp3,
    /// Internal Futureboard recording/cache format. Useful as a debug export
    /// target; not expected to be readable by general-purpose tools.
    Rauf,
}

impl AudioFileFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
            Self::Rauf => "rauf",
        }
    }

    /// File extension (without the dot) conventionally used for this format.
    pub fn extension(self) -> &'static str {
        self.as_str()
    }
}

impl std::fmt::Display for AudioFileFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Target sample representation inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSampleFormat {
    F32,
    I16,
    I24,
    I32,
}

impl AudioSampleFormat {
    pub fn bits(self) -> u16 {
        match self {
            Self::I16 => 16,
            Self::I24 => 24,
            Self::I32 | Self::F32 => 32,
        }
    }

    /// Bytes one sample occupies on disk for PCM containers.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 | Self::F32 => 4,
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32)
    }
}

/// Stream geometry shared by every encoder.
#[derive(Debug, Clone)]
pub struct AudioEncodeSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: AudioSampleFormat,
}

impl AudioEncodeSpec {
    pub fn validate(&self) -> Result<(), EncodeError> {
        if self.sample_rate == 0 {
            return Err(EncodeError::UnsupportedSpec(
                "sample_rate must be non-zero".to_string(),
            ));
        }
        if self.channels == 0 {
            return Err(EncodeError::UnsupportedSpec(
                "channels must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// Per-format encode summary returned by [`AudioEncoder::finalize`].
#[derive(Debug, Clone)]
pub struct AudioEncodeSummary {
    pub format: AudioFileFormat,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: AudioSampleFormat,
    pub frames_written: u64,
    pub bytes_written: u64,
}

// ---------------------------------------------------------------------------
// Per-format options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct WavEncodeOptions {
    /// Reserve room for an RF64 header (EBU Tech 3306) so the file may grow
    /// past the 4 GiB RIFF limit.
    ///
    /// The header carries a 36-byte `JUNK` chunk ahead of `fmt `, which every
    /// RIFF reader skips. While the data fits RIFF the file stays a plain
    /// RIFF/WAVE; once it does not, the header is rewritten in place as
    /// `RF64` with that chunk turned into `ds64`. Off by default: a file
    /// written without it keeps the classic 44-byte header.
    pub rf64: bool,
}

#[derive(Debug, Clone)]
pub struct FlacEncodeOptions {
    /// flacenc compression preset (0..=8). Higher = smaller/slower.
    pub compression_level: u8,
    /// Fixed block size in frames.
    pub block_size: usize,
}

impl Default for FlacEncodeOptions {
    fn default() -> Self {
        Self {
            compression_level: 5,
            block_size: 4096,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mp3Bitrate {
    Kbps128,
    Kbps192,
    Kbps256,
    Kbps320,
}

impl Mp3Bitrate {
    pub fn from_kbps(kbps: u32) -> Result<Self, EncodeError> {
        match kbps {
            128 => Ok(Self::Kbps128),
            192 => Ok(Self::Kbps192),
            256 => Ok(Self::Kbps256),
            320 => Ok(Self::Kbps320),
            other => Err(EncodeError::UnsupportedSpec(format!(
                "unsupported MP3 CBR bitrate {other} kbps (expected 128/192/256/320)"
            ))),
        }
    }

    pub fn kbps(self) -> u32 {
        match self {
            Self::Kbps128 => 128,
            Self::Kbps192 => 192,
            Self::Kbps256 => 256,
            Self::Kbps320 => 320,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Mp3EncodeOptions {
    pub bitrate: Mp3Bitrate,
    /// 0 (best/slowest) .. 9 (worst/fastest), LAME quality preset.
    pub quality: u8,
}

impl Default for Mp3EncodeOptions {
    fn default() -> Self {
        Self {
            bitrate: Mp3Bitrate::Kbps256,
            quality: 2,
        }
    }
}

/// Bundle of every per-format option plus metadata.
#[derive(Debug, Clone, Default)]
pub struct AudioEncodeOptions {
    pub format: AudioFileFormat,
    pub wav: WavEncodeOptions,
    pub flac: FlacEncodeOptions,
    pub mp3: Mp3EncodeOptions,
    pub metadata: AudioMetadata,
}

// ---------------------------------------------------------------------------
// Error model
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum EncodeError {
    Io(std::io::Error),
    UnsupportedFormat(AudioFileFormat),
    UnsupportedSpec(String),
    /// The format is recognized but its codec is not compiled into this build
    /// (e.g. MP3 without the `mp3` feature). Never silently faked.
    CodecUnavailable {
        format: AudioFileFormat,
        reason: String,
    },
    InvalidInput(String),
    FinalizeFailed(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::UnsupportedFormat(format) => write!(f, "unsupported format: {format}"),
            Self::UnsupportedSpec(message) => write!(f, "unsupported encode spec: {message}"),
            Self::CodecUnavailable { format, reason } => {
                write!(f, "{format} codec unavailable: {reason}")
            }
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::FinalizeFailed(message) => write!(f, "finalize failed: {message}"),
        }
    }
}

impl std::error::Error for EncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for EncodeError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

// ---------------------------------------------------------------------------
// Encoder trait + factory
// ---------------------------------------------------------------------------

/// A streaming PCM encoder. Frames are pushed interleaved; the encoder owns its
/// output file and finalizes the container on [`finalize`](AudioEncoder::finalize).
///
/// Implementations must not panic on bad input or codec errors — every failure
/// surfaces as an [`EncodeError`].
pub trait AudioEncoder: Send {
    fn format(&self) -> AudioFileFormat;
    fn spec(&self) -> AudioEncodeSpec;

    /// Push interleaved `[-1.0, 1.0]` float frames. Length must be a multiple of
    /// `channels`.
    fn write_interleaved_f32(&mut self, frames: &[f32]) -> Result<(), EncodeError>;

    /// Push interleaved full-scale signed 32-bit frames. Length must be a
    /// multiple of `channels`.
    fn write_interleaved_i32(&mut self, frames: &[i32]) -> Result<(), EncodeError>;

    /// Flush, patch headers/trailers, and close the file. Calling twice is a
    /// no-op-safe error rather than a panic.
    fn finalize(&mut self) -> Result<AudioEncodeSummary, EncodeError>;

    /// Make what has been written so far readable on disk, as if the stream
    /// ended here, without ending it: a process that dies before
    /// [`finalize`](AudioEncoder::finalize) then leaves a file that opens up
    /// to the last checkpoint. Blocks on file I/O; never call it from a
    /// realtime thread. Formats that cannot do this cheaply do nothing.
    fn checkpoint(&mut self) -> Result<(), EncodeError> {
        Ok(())
    }
}

/// Whether MP3 encoding is compiled into this build (the `mp3` feature). The UI
/// uses this to show MP3 as available or disabled with a clear explanation —
/// never to silently fall back.
pub fn mp3_available() -> bool {
    cfg!(feature = "mp3-lame")
}

/// Whether FLAC encoding is compiled into this build (the `flac` feature).
pub fn flac_available() -> bool {
    cfg!(feature = "flac")
}

/// Construct an encoder for `path` from a spec + options.
///
/// Returns [`EncodeError::CodecUnavailable`] (not a panic, not a fake success)
/// when a format's codec is not compiled into this build.
pub fn create_encoder(
    path: impl AsRef<Path>,
    spec: AudioEncodeSpec,
    options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    spec.validate()?;
    let path = path.as_ref();
    match options.format {
        AudioFileFormat::Wav => Ok(Box::new(crate::wav::WavEncoder::create(
            path,
            spec,
            options.wav,
        )?)),
        AudioFileFormat::Rauf => Ok(Box::new(crate::rauf::RaufEncoder::create(path, spec)?)),
        AudioFileFormat::Flac => create_flac_encoder(path, spec, options),
        AudioFileFormat::Mp3 => create_mp3_encoder(path, spec, options),
    }
}

#[cfg(feature = "flac")]
fn create_flac_encoder(
    path: &Path,
    spec: AudioEncodeSpec,
    options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    Ok(Box::new(crate::flac::FlacEncoder::create(
        path,
        spec,
        options.flac,
        options.metadata,
    )?))
}

#[cfg(not(feature = "flac"))]
fn create_flac_encoder(
    _path: &Path,
    _spec: AudioEncodeSpec,
    _options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    Err(EncodeError::CodecUnavailable {
        format: AudioFileFormat::Flac,
        reason: "this build was compiled without the `flac` feature".to_string(),
    })
}

#[cfg(feature = "mp3-lame")]
fn create_mp3_encoder(
    path: &Path,
    spec: AudioEncodeSpec,
    options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    Ok(Box::new(crate::mp3::Mp3Encoder::create(
        path,
        spec,
        options.mp3,
        options.metadata,
    )?))
}

#[cfg(not(feature = "mp3-lame"))]
fn create_mp3_encoder(
    _path: &Path,
    _spec: AudioEncodeSpec,
    _options: AudioEncodeOptions,
) -> Result<Box<dyn AudioEncoder>, EncodeError> {
    Err(EncodeError::CodecUnavailable {
        format: AudioFileFormat::Mp3,
        reason: "MP3 export is not available in this build (`mp3` feature disabled)".to_string(),
    })
}

// ---------------------------------------------------------------------------
// Shared sample conversion helpers
// ---------------------------------------------------------------------------

/// Validate that an interleaved buffer length divides evenly by `channels`.
pub(crate) fn check_interleaved_len(len: usize, channels: u16) -> Result<(), EncodeError> {
    let channels = channels as usize;
    if channels == 0 {
        return Err(EncodeError::UnsupportedSpec("channels is zero".to_string()));
    }
    if !len.is_multiple_of(channels) {
        return Err(EncodeError::InvalidInput(format!(
            "interleaved sample count {len} is not divisible by channels {channels}"
        )));
    }
    Ok(())
}

/// Full-scale `[-1, 1]` float to signed 32-bit. Clamps; no dither (first pass).
#[inline]
pub(crate) fn f32_to_i32(x: f32) -> i32 {
    let v = (x.clamp(-1.0, 1.0) as f64) * (i32::MAX as f64);
    v.round() as i32
}

#[inline]
pub(crate) fn f32_to_i16(x: f32) -> i16 {
    let v = (x.clamp(-1.0, 1.0) as f64) * (i16::MAX as f64);
    v.round() as i16
}

/// Float to signed 24-bit value stored in the low bytes of an `i32`.
#[inline]
pub(crate) fn f32_to_i24(x: f32) -> i32 {
    let v = (x.clamp(-1.0, 1.0) as f64) * 8_388_607.0;
    v.round() as i32
}

#[inline]
pub(crate) fn i32_to_f32(x: i32) -> f32 {
    (x as f64 / (i32::MAX as f64 + 1.0)) as f32
}

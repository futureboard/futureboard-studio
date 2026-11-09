//! Futureboard audio encoding and internal media formats.
//!
//! SphereEncoder's single responsibility is turning interleaved PCM frames into
//! a valid container on disk (WAV, FLAC, Ogg Opus, RAUF). It contains no
//! FFmpeg/libav dependency and never spawns an external converter process.
//! Arrangement rendering lives in the audio engine; building an export request
//! from project state lives in the UI/layout layer.

pub mod flac_header;
pub mod format;
pub mod metadata;
pub mod rauf;
pub mod wav;

#[cfg(feature = "flac")]
pub mod flac;

#[cfg(feature = "opus")]
pub mod ogg_opus;

pub use format::{
    AudioDither, AudioEncodeOptions, AudioEncodeSpec, AudioEncodeSummary, AudioEncoder,
    AudioFileFormat, AudioSampleFormat, EncodeError, FlacEncodeOptions, OPUS_SAMPLE_RATE,
    OpusBitrate, OpusEncodeOptions, WavEncodeOptions, create_encoder, flac_available,
    opus_available,
};
pub use metadata::AudioMetadata;

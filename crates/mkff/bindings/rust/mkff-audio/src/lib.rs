//! # MKFF audio
//!
//! MKFF's own audio stack, written from the specifications in safe Rust, with no third-party
//! codec code:
//!
//! | Codec | Decode | Encode | Crate |
//! |-------|--------|--------|-------|
//! | MP3 (MPEG-1/2/2.5 layers I, II, III) | ✓ | layer III | `mkff-mp3` |
//! | AAC-LC (HE-AAC: LC core, upsampled) | ✓ | ✓ | `mkff-aac` |
//! | FLAC | ✓ (bit-exact) | ✓ | `mkff-flac` |
//! | ALAC | ✓ (bit-exact) | ✓ | `mkff-alac` |
//! | AC-3 / E-AC-3 | ✓ | — | `mkff-ac3` |
//! | Opus (SILK / CELT / hybrid, multistream) | ✓ | — | `mkff-opus` |
//! | Vorbis | ✓ | — | `mkff-vorbis` |
//! | PCM | ✓ | ✓ | `mkff-audio-format` |
//!
//! Containers (`mkff-audio-format`): WAV / RF64 / AIFF, raw MPEG audio (ID3, Xing / LAME
//! gapless), ADTS, FLAC, Ogg (Vorbis / Opus / FLAC), MP4 / M4A / MOV audio tracks (fragmented
//! too); writers for all of them.
//!
//! - [`AudioFile`]: open a file, stream it, seek sample-exactly, read any range at any rate.
//! - [`AudioWriter`]: encode into `.mp3`, `.m4a`, `.aac`, `.flac` or `.wav`.
//! - [`codecs`]: decoders / encoders for elementary streams.
//! - [`addon`]: everything above as Media Kit flavors ([`addon::roster`] gives the process
//!   roster with them registered next to the GPU accelerants).

#![forbid(unsafe_code)]

pub mod addon;
pub mod codecs;
mod file;
mod writer;

pub use codecs::{decoder, decoder_for, encoder, EncoderSettings};
pub use file::AudioFile;
pub use mkff_audio_core::{
    resample, AudioBuffer, CodecId, Decoder, Encoder, Error, Packet, Result,
};
pub use mkff_audio_format as format;
pub use writer::{AudioWriter, Container};

/// File extensions [`AudioFile::open`] reads.
pub fn supported_extensions() -> &'static [&'static str] {
    mkff_audio_format::AUDIO_EXTENSIONS
}

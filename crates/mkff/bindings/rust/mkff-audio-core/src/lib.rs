//! Shared building blocks of the MKFF audio codecs (`mkff-mp3`, `mkff-aac`, `mkff-flac`,
//! `mkff-alac`, `mkff-ac3`, `mkff-opus`, `mkff-vorbis`) and containers (`mkff-audio-format`).
//!
//! Everything here is written for MKFF from the public specifications, in safe Rust, with no
//! dependencies:
//!
//! - [`bits`]: MSB-first and LSB-first bit readers, an MSB-first bit writer.
//! - [`fft`] / [`mdct`]: mixed-radix complex FFT (any length with factors 2, 3, 5 and others) and
//!   an MDCT / IMDCT of any even size built on it.
//! - [`crc`]: the CRC-8 / CRC-16 / CRC-32 variants the formats use.
//! - [`resample`]: band-limited sample-rate conversion.
//! - [`codec`]: the [`Decoder`] / [`Encoder`] traits every codec crate implements.

#![forbid(unsafe_code)]

pub mod bits;
pub mod buffer;
pub mod codec;
pub mod crc;
mod error;
pub mod fft;
pub mod mdct;
pub mod resample;

pub use buffer::AudioBuffer;
pub use codec::{CodecId, Decoder, Encoder, Packet};
pub use error::{Error, Result};

//! Apple Lossless (ALAC) decoder and encoder, written for MKFF in safe Rust from Apple's
//! published ALAC format description.
//!
//! - [`AlacConfig`]: the magic cookie (`ALACSpecificConfig`, optionally wrapped in `frma` /
//!   `alac` atom headers and followed by a `chan` atom).
//! - [`AlacDecoder`]: one packet = one ALAC frame; f32 output through
//!   [`mkff_audio_core::Decoder`], bit-exact integers through [`AlacDecoder::decode_i32`].
//! - [`AlacEncoder`]: implements [`mkff_audio_core::Encoder`]; `config()` is the magic cookie.
//!
//! Supported: 16 / 20 / 24 / 32-bit, 1..=8 channels (SCE / CPE / LFE elements in ALAC's default
//! layouts, output in WAV order), adaptive Golomb-Rice residuals with zero runs, the adaptive
//! FIR predictor (including the first-order pre-pass of mode 15), stereo un-mixing, shifted-off
//! low bytes, uncompressed (escape) elements and partial frames. Coupling (CCE) and program
//! config (PCE) elements are rejected; DSE and FIL elements are skipped.

#![forbid(unsafe_code)]

mod config;
mod decoder;
mod encoder;
mod predictor;

pub use config::{AlacConfig, CONFIG_LEN, MAX_FRAME_LENGTH};
pub use decoder::AlacDecoder;
pub use encoder::{AlacEncoder, AlacEncoderConfig};

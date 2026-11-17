//! FLAC decoder and encoder, written for MKFF in safe Rust from RFC 9639.
//!
//! - [`StreamInfo`]: the `STREAMINFO` metadata block (parse / serialize).
//! - [`parse_frame_header`] / [`find_frame_sync`]: frame header parsing and resynchronisation.
//! - [`FlacDecoder`]: one packet = one FLAC frame; f32 output through
//!   [`mkff_audio_core::Decoder`], bit-exact integers through [`FlacDecoder::decode_i32`].
//! - [`FlacEncoder`]: fixed and LPC predictors, stereo decorrelation, partitioned Rice coding,
//!   STREAMINFO with MD5; implements [`mkff_audio_core::Encoder`].
//! - [`Md5`]: the RFC 1321 digest FLAC uses for its audio signature.
//!
//! Native `.flac` files are `fLaC`, metadata blocks (the first is `STREAMINFO`), then frames;
//! splitting a file into frames is a container's job (frames carry no length field: the next
//! frame starts at the next sync code whose header CRC-8 checks out and the frame CRC-16 ends
//! where the next begins).

#![forbid(unsafe_code)]

mod decoder;
mod encoder;
mod frame;
mod lpc;
mod md5;
mod streaminfo;

pub use decoder::FlacDecoder;
pub use encoder::{FlacEncoder, FlacEncoderConfig};
pub use frame::{find_frame_sync, parse_frame_header, ChannelAssignment, FrameHeader};
pub use md5::Md5;
pub use streaminfo::{StreamInfo, STREAMINFO_LEN};

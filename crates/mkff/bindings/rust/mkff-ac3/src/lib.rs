//! AC-3 and E-AC-3 decoder, written for MKFF in safe Rust
//!
//! Implemented from ETSI TS 102 366 (ATSC A/52), including Annex D (alternate bit stream
//! syntax), Annex E (Enhanced AC-3) and Annex F (ISO BMFF `dac3` / `dec3`).
//!
//! - [`parse_frame`] / [`find_sync`] / [`check_crc`]: syncframe headers ([`FrameInfo`]).
//! - [`parse_dac3`] / [`parse_dec3`]: the MP4 configuration boxes, giving the real channel
//!   count and layout (MP4 sample entries always say 2 channels).
//! - [`Ac3Decoder`]: the decoder ([`mkff_audio_core::Decoder`]), with optional Lo/Ro
//!   [`Downmix`] and dynamic range control.
//!
//! Decoding covers AC-3 `bsid` 0..=10 (including the half / quarter rate `bsid` 9 / 10) and
//! E-AC-3 `bsid` 11..=16: exponent strategies, parametric bit allocation with delta bit
//! allocation, grouped mantissas, coupling with phase flags, rematrixing, dither, block
//! switching, LFE, CRC checks, the E-AC-3 frame syntax with 1 / 2 / 3 / 6 blocks, dependent
//! substreams with custom channel maps (7.1 and beyond), the adaptive hybrid transform (vector
//! quantization and gain adaptive quantization) and spectral extension. Not implemented:
//! E-AC-3 enhanced coupling (frames using it fail with [`mkff_audio_core::Error::Unsupported`])
//! and transient pre-noise processing (its data is parsed and ignored, which the specification
//! allows as the audio is still correct, only without the pre-echo reduction).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bitalloc;
mod decoder;
#[cfg(test)]
mod devtools;
mod frame;
mod header;
mod imdct;
pub mod layout;
mod tables;
mod vq;

pub use decoder::{Ac3Decoder, Downmix};
pub use header::{
    check_crc, find_sync, parse_dac3, parse_dec3, parse_frame, Dac3, Dec3, Dec3Substream,
    FrameInfo, StreamType, SyncInfo, SYNCWORD,
};
pub use layout::Channel;

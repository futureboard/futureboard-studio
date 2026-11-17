//! Vorbis I decoder, written for MKFF in safe Rust from the Xiph.Org Vorbis I specification.
//!
//! - [`IdentHeader`] / [`Comments`]: the identification and comment headers.
//! - [`split_config`]: split a container's codec configuration into the three headers.
//! - [`VorbisDecoder`]: the packet decoder (implements [`mkff_audio_core::Decoder`]).
//!
//! The setup header is fully supported: codebooks (lookup types 0, 1 and 2, ordered / sparse
//! codeword lists, single-entry books), floor types 0 and 1, residue types 0, 1 and 2, mappings
//! with channel coupling and submaps, and any number of modes.

#![forbid(unsafe_code)]

mod bitread;
mod codebook;
mod decoder;
mod floor;
mod header;
mod imdct;
mod residue;
mod setup;
#[cfg(test)]
mod testutil;

pub use decoder::VorbisDecoder;
pub use header::{split_config, Comments, IdentHeader};

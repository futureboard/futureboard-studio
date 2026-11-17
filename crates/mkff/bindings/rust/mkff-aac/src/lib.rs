//! AAC-LC decoder and encoder (raw, ADTS, LATM framing helpers), written for MKFF in safe Rust
//! from ISO/IEC 14496-3 (subpart 4, General Audio) and ISO/IEC 13818-7.
//!
//! - [`AudioSpecificConfig`] / [`ProgramConfig`]: decoder configuration parsing and writing,
//!   including explicit-rate escapes and implicit / hierarchical / backward-compatible SBR and
//!   PS signalling.
//! - [`parse_adts`] / [`adts_header`]: ADTS framing; [`LatmParser`]: LOAS / LATM.
//! - [`AacDecoder`]: AAC-LC decoding (all window sequences and shapes, M/S, intensity stereo,
//!   PNS, TNS, pulse data, PCE layouts, channel configurations 1–7). HE-AAC streams decode
//!   their AAC-LC core, interpolated to the SBR rate.
//! - [`AacEncoder`]: AAC-LC encoding (block switching, psychoacoustic model, rate control
//!   with a bit reservoir, M/S, TNS, PNS).
//!
//! # Scope
//!
//! Decoder: AAC-LC with SCE / CPE / LFE / DSE / PCE / FIL / END elements, the eleven spectral
//! codebooks with escapes, pulse data, intensity stereo, M/S, PNS and TNS on long and short
//! windows. Coupling channel elements are parsed and dropped (their contribution is not
//! mixed in). Not supported, rejected with [`Error::Unsupported`](mkff_audio_core::Error):
//! AAC Main, SSR and LTP object types, the error-resilient object types, 960-sample frames,
//! scalable (core coder) configurations. The SBR and PS payloads of HE-AAC streams are not
//! decoded: the core is interpolated 2× (and duplicated to stereo for PS) so that the output
//! has the HE-AAC rate, length and channel count, but no reconstructed high band.
//!
//! Encoder: AAC-LC, raw access units (plus an ADTS helper), sine windows, no intensity
//! stereo, no pulse data.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod adts;
mod asc;
mod decoder;
mod encoder;
mod filterbank;
mod huffman;
mod huffman_tables;
mod ics;
mod latm;
mod tables;

pub use adts::{adts_header, parse_adts, AdtsHeader};
pub use asc::{object_type, AudioSpecificConfig, ProgramConfig, SbrSignalling};
pub use decoder::{AacDecoder, DecoderStats};
pub use encoder::{AacEncoder, AacEncoderConfig};
pub use latm::{loas_frame_length, LatmParser};
pub use tables::{sampling_index, SAMPLE_RATES};

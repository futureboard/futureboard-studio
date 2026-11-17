//! MPEG-1/2/2.5 audio layers I, II and III decoder and a Layer III (MP3) encoder, written for
//! MKFF in safe Rust from ISO/IEC 11172-3, ISO/IEC 13818-3 and the MPEG-2.5 extension.
//!
//! # Decoding
//!
//! [`Mp3Decoder`] implements [`mkff_audio_core::Decoder`]: feed it packets of one or more
//! complete frames (or arbitrary byte chunks: partial frames are buffered) and get planar
//! `f32` at the stream's rate. It handles all layers, versions and channel modes (M/S and
//! intensity stereo, including MPEG-2 LSF intensity stereo), short and mixed blocks, the
//! Layer III bit reservoir across packets, CRCs (checked on request) and free format.
//!
//! ```
//! use mkff_audio_core::Decoder;
//! use mkff_mp3::Mp3Decoder;
//!
//! # fn run(file: &[u8]) -> mkff_audio_core::Result<()> {
//! let mut dec = Mp3Decoder::new();
//! let pcm = dec.decode(file)?;
//! println!("{} Hz, {} channels, {} samples", pcm.sample_rate, pcm.channels.len(), pcm.frames());
//! # Ok(())
//! # }
//! ```
//!
//! [`parse_header`] / [`find_sync`] split streams into frames; [`parse_xing`] reads the Xing /
//! Info header with the LAME extension (encoder delay and padding for gapless playback) or a
//! VBRI header from the first frame.
//!
//! # Encoding
//!
//! [`Mp3Encoder`] implements [`mkff_audio_core::Encoder`]: constant bit rate Layer III at
//! 32 / 44.1 / 48 kHz (MPEG-1), 16 / 22.05 / 24 kHz (MPEG-2) and 8 / 11.025 / 12 kHz
//! (MPEG-2.5), mono, stereo or joint stereo, with a psychoacoustic model, block switching and
//! the bit reservoir. Each packet is one frame. After [`mkff_audio_core::Encoder::flush`],
//! [`Mp3Encoder::xing_frame`] builds an Info header frame carrying the frame / byte counts and
//! the LAME-style delay / padding so that players trim the output to the input exactly.
//!
//! ```
//! use mkff_audio_core::Encoder;
//! use mkff_mp3::{Mp3Encoder, Mp3EncoderConfig};
//!
//! let mut enc = Mp3Encoder::new(Mp3EncoderConfig::new(44100, 2, 128)).unwrap();
//! let left = vec![0.0f32; 44100];
//! let right = vec![0.0f32; 44100];
//! let mut file = Vec::new();
//! let mut frames = enc.encode(&[&left, &right]).unwrap();
//! frames.extend(enc.flush().unwrap());
//! file.extend(enc.xing_frame());
//! for p in &frames {
//!     file.extend_from_slice(&p.data);
//! }
//! assert_eq!(enc.priming(), 1057);
//! ```

#![forbid(unsafe_code)]
// DSP loops index several arrays with one counter; iterator chains would obscure them
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

mod decoder;
mod encoder;
mod header;
mod huffman;
mod layer12;
mod layer3;
mod synth;
mod tables;
#[cfg(test)]
mod testgen;
mod xing;

pub use decoder::Mp3Decoder;
pub use encoder::{Mp3Encoder, Mp3EncoderConfig, StereoMode, StreamStats};
pub use header::{find_sync, parse_header, ChannelMode, FrameHeader, MpegVersion};
pub use xing::{
    crc16_audio, parse_xing, xing_frame, XingFrameInfo, XingInfo, XingKind, DECODER_DELAY,
};

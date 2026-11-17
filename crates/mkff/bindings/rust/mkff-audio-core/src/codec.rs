//! The codec traits every MKFF audio codec crate implements.

use crate::{AudioBuffer, Result};

/// Audio codecs MKFF knows (decoders and / or encoders exist in the codec crates).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CodecId {
    /// Integer / float PCM (see the container for the sample format).
    Pcm,
    /// MPEG-1 / 2 / 2.5 audio layers I, II, III.
    MpegAudio,
    /// AAC (LC object type; HE-AAC streams decode their AAC-LC core).
    Aac,
    Flac,
    Alac,
    /// AC-3 and E-AC-3.
    Ac3,
    Opus,
    Vorbis,
}

impl CodecId {
    pub fn name(self) -> &'static str {
        match self {
            CodecId::Pcm => "pcm",
            CodecId::MpegAudio => "mpeg-audio",
            CodecId::Aac => "aac",
            CodecId::Flac => "flac",
            CodecId::Alac => "alac",
            CodecId::Ac3 => "ac3",
            CodecId::Opus => "opus",
            CodecId::Vorbis => "vorbis",
        }
    }
}

/// One encoded packet (access unit).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    /// Presentation time in samples at the codec's sample rate (the first packet's first sample is
    /// 0; encoder priming makes the decoded output start [`Encoder::priming`] samples early).
    pub pts: i64,
    /// Samples this packet decodes to.
    pub duration: u32,
}

/// A stateful packet decoder for one elementary stream.
///
/// Each codec crate documents what a packet and the configuration (`new`) are for its codec.
pub trait Decoder: Send {
    /// Decode one packet into planar f32. May return an empty buffer (a header packet, or
    /// output delayed by the codec).
    fn decode(&mut self, packet: &[u8]) -> Result<AudioBuffer>;
    /// Forget inter-packet state (after a seek).
    fn reset(&mut self);
    /// Output sample rate (0 until known).
    fn sample_rate(&self) -> u32;
    /// Output channel count (0 until known).
    fn channels(&self) -> usize;
}

/// An audio encoder: planar f32 in, packets out.
pub trait Encoder: Send {
    /// Feed planar samples (one slice per channel, equal lengths, any length). Returns the
    /// packets completed so far.
    fn encode(&mut self, input: &[&[f32]]) -> Result<Vec<Packet>>;
    /// Encode what is buffered (zero-padded) and return the last packets.
    fn flush(&mut self) -> Result<Vec<Packet>>;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> usize;
    /// Samples per packet (0 when variable).
    fn frame_size(&self) -> usize;
    /// Samples of encoder delay at the start of the decoded output.
    fn priming(&self) -> u32;
    /// The decoder configuration a container stores (`AudioSpecificConfig`, FLAC `STREAMINFO`,
    /// …); empty when the codec has none.
    fn config(&self) -> Vec<u8>;
}

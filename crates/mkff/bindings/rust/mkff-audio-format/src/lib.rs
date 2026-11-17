//! Audio container demuxers and muxers for MKFF: WAV / RF64 / AIFF, raw MPEG audio, ADTS, FLAC,
//! Ogg and MP4 / M4A / MOV audio tracks.
//!
//! Everything is written for MKFF from the public specifications, in safe Rust, with no
//! dependencies besides [`mkff_audio_core`]. Codecs live in the sibling crates; this crate only
//! parses the few codec header fields a container needs (frame sizes, durations, decoder
//! configurations).
//!
//! # Reading
//!
//! ```no_run
//! # fn main() -> mkff_audio_core::Result<()> {
//! let mut demux = mkff_audio_format::open_path("song.m4a")?;
//! let track = demux.tracks()[0].clone();
//! println!("{} {} Hz x{} {:?}", track.name, track.sample_rate, track.channels, track.duration);
//! while let Some(pkt) = demux.next_packet()? {
//!     // feed pkt.data to the decoder for track.codec, configured with track.config
//! #   let _ = pkt;
//! }
//! # Ok(()) }
//! ```
//!
//! [`open`] sniffs the content (the file name only breaks ties) and returns a [`Demuxer`]:
//!
//! | `format_name()` | Files | Packets |
//! |---|---|---|
//! | `"WAV"` | RIFF WAVE, RF64, BW64 (PCM 8/16/24/32, float 32/64, A-law, µ-law, `WAVE_FORMAT_EXTENSIBLE`) | ~4096 frames of interleaved PCM |
//! | `"AIFF"` | AIFF, AIFF-C (`NONE twos sowt fl32 fl64 in24 in32 raw alaw ulaw`) | ~4096 frames of interleaved PCM |
//! | `"MP3"` | raw MPEG-1/2/2.5 layer I/II/III (`.mp3 .mp2 .mp1`), ID3v2/ID3v1/APE skipped | one frame (with header) |
//! | `"ADTS"` | raw AAC in ADTS | one raw AAC access unit (ADTS header and CRC stripped) |
//! | `"FLAC"` | native FLAC | one FLAC frame (with header) |
//! | `"Ogg"` | Ogg Vorbis, Opus, FLAC (several logical streams) | one codec packet |
//! | `"MP4"` | MP4, M4A, M4B, MOV, 3GP, fragmented MP4 (audio tracks) | one MP4 sample (PCM: ~4096 frames) |
//!
//! ## Timestamps, priming and duration
//!
//! [`DemuxPacket::pts`] and [`DemuxPacket::duration`] are in [`Track::time_base`] units per
//! second (equal to the sample rate everywhere except MP4 tracks whose media timescale is
//! something else). They describe the *decoder output* timeline: decoding every packet from the
//! start produces samples `0, 1, 2, …`, and the packet with `pts = p` produces the samples from
//! `p` on. Encoders add delay, so a player should:
//!
//! 1. drop the first [`Track::priming`] decoded samples,
//! 2. play [`Track::duration`] samples (when known; [`Track::padding`] is the matching count of
//!    samples to drop at the end when the container records it).
//!
//! Presentation time `t` (in samples) is therefore pts `t + priming`. Per format, `priming` comes
//! from:
//!
//! - MP4 / MOV: the first non-empty edit's `media_time` (falling back to `iTunSMPB` for AAC; the
//!   `dOps` pre-skip for Opus when larger); `duration` from the edit's segment duration.
//! - MP3: the LAME tag: `encoder delay + 529`, the 529 being the decoder delay of the standard
//!   layer III synthesis filterbank (the convention FFmpeg and gapless players use), and
//!   `padding - 529` at the end. MP3 without a LAME tag: 0.
//! - Ogg Opus: the pre-skip; Ogg Vorbis: 0, unless the first page's granule position says the
//!   stream starts with samples to discard. Ogg `duration` / `padding` come from the last
//!   granule position. Packet pts come from granule positions (a stream cut out of a longer one
//!   starts at a pts other than 0).
//! - ADTS, WAV, AIFF, FLAC: 0 (ADTS records no encoder delay; AAC encoders typically add 1024 or
//!   2112 samples).
//!
//! The first Vorbis packet (and the first after a seek) has duration 0: a Vorbis decoder outputs
//! nothing for it.
//!
//! ## Seeking
//!
//! [`Demuxer::seek`] takes a pts (same units as packet pts, i.e. including priming) and positions
//! the demuxer so that the next packets of that track start at or before it; it returns the pts
//! of the first packet that will be returned. Decoders that need pre-roll (Opus: 80 ms, Vorbis /
//! AAC / MP3: a frame or two) should be fed a little earlier: seek to `target - preroll` and
//! discard decoded samples before `target`. Accuracy:
//!
//! - WAV, AIFF, PCM in MP4: the exact sample.
//! - MP4, FLAC, ADTS: the packet containing the target (FLAC via the SEEKTABLE when present,
//!   else bisection on frame headers; ADTS by scanning headers, building an index lazily).
//! - MP3: the frame containing the target when the frames up to it are indexed (files up to 8 MiB
//!   are indexed at open; seeks scan up to 16 MiB further). Beyond that the Xing TOC / VBRI table
//!   / CBR bit rate gives an estimated position, and the returned pts is an estimate.
//! - Ogg: bisection on granule positions; lands on the first packet of a page that starts at or
//!   before the target (usually less than a page early).
//!
//! ## Decoder configuration
//!
//! [`Track::config`] carries what the codec crate's decoder needs:
//!
//! | Codec | `config` |
//! |---|---|
//! | AAC | `AudioSpecificConfig` (from `esds`, or built from the first ADTS header) |
//! | FLAC | the 34-byte `STREAMINFO` block body |
//! | ALAC | the 24-byte `ALACSpecificConfig` ("magic cookie") |
//! | Opus | an `OpusHead` packet (RFC 7845 §5.1; built from `dOps` in MP4) |
//! | Vorbis | the three header packets, Xiph-laced as in Matroska `CodecPrivate`: `[2]`, laced sizes of id and comment headers, then id, comment, setup ([`xiph_unlace`] splits it) |
//! | AC-3 / E-AC-3 | the `dac3` / `dec3` box payload (MP4) |
//! | MP3, PCM | empty (PCM: see [`Track::pcm`] and [`PcmDecoder`]) |
//!
//! ## Multi-track files
//!
//! Ogg and MP4 files can hold several audio tracks; [`Demuxer::next_packet`] returns them
//! interleaved in file order (MP4: by sample offset). Video and other non-audio tracks are
//! skipped, as are audio tracks with codecs MKFF has no decoder for.
//!
//! # Writing
//!
//! - [`WavWriter`]: PCM 16/24/32 and float WAV, `WAVE_FORMAT_EXTENSIBLE` with a channel mask for
//!   more than 2 channels or more than 16 bits, RF64 when the file passes 4 GiB.
//! - [`MpaWriter`] ([`Mp3Writer`]): raw MPEG audio frames, optionally behind a Xing/Info + LAME
//!   tag frame ([`MpaInfoTag`]) that records the frame count, a seek TOC and the gapless delay /
//!   padding.
//! - [`AdtsWriter`]: raw AAC access units in ADTS (from an `AudioSpecificConfig`).
//! - [`FlacWriter`]: `fLaC`, STREAMINFO (rewritten at the end), SEEKTABLE, PADDING, frames.
//! - [`OggWriter`]: generic Ogg pages for one logical stream (Vorbis, FLAC via
//!   [`ogg_flac_headers`], anything with caller-supplied granule positions), and
//!   [`OggOpusWriter`] for Opus.
//! - [`M4aWriter`]: audio-only MP4 / M4A for AAC, ALAC, FLAC, Opus, MP3 and PCM, with edit list
//!   (and `iTunSMPB` for AAC) for gapless playback, optional fast start.
//!
//! All writers take a `std::io::Write` (plus `Seek` where headers are patched at the end) and
//! return the inner writer from `finish`.
//!
//! # Robustness
//!
//! Readers never panic on malformed input; they return [`Error::InvalidData`] /
//! [`Error::EndOfData`] / [`Error::Unsupported`], skip the damage or stop at it. Allocations
//! driven by sizes in the file are bounded by the file length or fixed limits (MP4 `moov` 256 MiB,
//! `moof` 64 MiB, one packet 64 MiB, an Ogg packet 16 MiB). Large files stream: only indexes (MP4
//! sample tables, MP3 / ADTS frame offsets seen so far) are kept in memory.
//!
//! # Not supported
//!
//! Free-format MPEG audio; ADTS frames with several raw data blocks and no CRC; chained Ogg
//! files (only the first link's streams are read); MP4 tracks whose sample description changes
//! mid-track (the first `stsd` entry is used), compressed QuickTime sound (`ima4`, …), `ctts`
//! (audio has none in practice), encrypted tracks; Wave64, CAF, Matroska, MPEG-TS.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::Path;

pub use mkff_audio_core::{AudioBuffer, CodecId, Error, Result};

mod adts;
mod aiff;
mod flac;
mod id3;
mod io;
mod mp4;
mod mpa;
mod ogg;
mod pcm;
mod wav;
mod xiph;

pub use adts::{adts_header, AdtsWriter};
pub use flac::{FlacWriter, FlacWriterOptions};
pub use mp4::{M4aTrack, M4aWriter};
pub use mpa::{MpaGapless, MpaInfoTag, MpaWriter, MpegAudioHeader};
pub use ogg::{ogg_flac_headers, OggOpusWriter, OggWriter};
pub use pcm::{decode_pcm, encode_pcm, PcmCoding, PcmDecoder, PcmFormat};
pub use wav::{default_channel_mask, WavSampleFormat, WavWriter};
pub use xiph::{opus_packet_duration, xiph_lace, xiph_unlace};

/// Alias of [`MpaWriter`] (raw MPEG audio is usually MP3).
pub type Mp3Writer<W> = MpaWriter<W>;

/// A readable, seekable byte stream that can move between threads. Implemented for every
/// `Read + Seek + Send` type (files, `Cursor<Vec<u8>>`, …).
pub trait ReadSeek: std::io::Read + std::io::Seek + Send {}

impl<T: std::io::Read + std::io::Seek + Send> ReadSeek for T {}

/// One audio track (elementary stream) of a container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// The codec of the packets.
    pub codec: CodecId,
    /// Output sample rate in Hz where the container knows it (Opus: always 48000; HE-AAC in MP4:
    /// the SBR output rate signalled in the `AudioSpecificConfig` — an AAC-LC-core decoder
    /// outputs half of it; ADTS cannot signal SBR, so it is the core rate there).
    pub sample_rate: u32,
    /// Output channel count (AC-3 / E-AC-3 in MP4: from `dac3` / `dec3`; AAC with
    /// `channelConfiguration` 0: 0 when the program config element was not parsed).
    pub channels: u32,
    /// Significant bits per sample when the container records it (PCM, FLAC, ALAC).
    pub bits_per_sample: Option<u32>,
    /// The sample layout for [`CodecId::Pcm`] tracks.
    pub pcm: Option<PcmFormat>,
    /// WAV-style channel mask (`SPEAKER_FRONT_LEFT` = 1, …) when the container gives one.
    pub channel_mask: Option<u32>,
    /// Decoder configuration (see the crate docs for the per-codec contents).
    pub config: Vec<u8>,
    /// Units per second of packet `pts` / `duration` and of [`Track::duration`].
    pub time_base: u32,
    /// Playable duration in `time_base` units after removing priming and padding, when known.
    pub duration: Option<u64>,
    /// Decoded samples (at `sample_rate`) to drop at the start (encoder delay).
    pub priming: u64,
    /// Decoded samples (at `sample_rate`) to drop at the end, when known (0 otherwise; use
    /// `duration` to trim).
    pub padding: u64,
    /// A codec label for user interfaces: `"PCM s16le"`, `"MP3"`, `"AAC-LC"`, `"HE-AAC"`,
    /// `"FLAC"`, `"ALAC"`, `"Opus"`, `"Vorbis"`, `"AC-3"`, `"E-AC-3"`, …
    pub name: String,
}

impl Track {
    /// A track with the given basics and everything else empty / unknown
    /// (`time_base = sample_rate`).
    pub fn new(codec: CodecId, sample_rate: u32, channels: u32, name: impl Into<String>) -> Self {
        Self {
            codec,
            sample_rate,
            channels,
            bits_per_sample: None,
            pcm: None,
            channel_mask: None,
            config: Vec::new(),
            time_base: sample_rate,
            duration: None,
            priming: 0,
            padding: 0,
            name: name.into(),
        }
    }

    /// Playable duration in seconds, when known.
    pub fn duration_secs(&self) -> Option<f64> {
        let d = self.duration?;
        (self.time_base > 0).then(|| d as f64 / f64::from(self.time_base))
    }
}

/// One compressed packet read from a container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DemuxPacket {
    /// Index into [`Demuxer::tracks`].
    pub track: usize,
    /// The packet payload (see the crate docs for what a packet is per format).
    pub data: Vec<u8>,
    /// Presentation time of the first decoded sample, in the track's `time_base` units (decoder
    /// output timeline; see the crate docs on priming).
    pub pts: i64,
    /// Decoded duration in `time_base` units (0 when unknown, e.g. the first Vorbis packet).
    pub duration: u32,
    /// Whether decoding can start at this packet (true for almost all audio packets).
    pub keyframe: bool,
}

/// A container reader.
pub trait Demuxer: Send {
    /// `"WAV"`, `"AIFF"`, `"MP3"`, `"ADTS"`, `"FLAC"`, `"Ogg"` or `"MP4"`.
    fn format_name(&self) -> &str;
    /// The audio tracks (at least one).
    fn tracks(&self) -> &[Track];
    /// The next packet in file order (all tracks interleaved), or `None` at the end.
    fn next_packet(&mut self) -> Result<Option<DemuxPacket>>;
    /// Seek so the next packets of `track` start at or before `pts` (time_base units); returns
    /// the pts of the first packet that will be returned. Decoders need pre-roll, so landing
    /// earlier is fine. Other tracks of the file are repositioned near the same time.
    fn seek(&mut self, track: usize, pts: i64) -> Result<i64>;
}

/// File extensions (lower case, without the dot) of the formats [`open`] reads.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "rf64", "bw64", "aif", "aiff", "aifc", "mp3", "mp2", "mp1", "mpa", "aac",
    "adts", "flac", "ogg", "oga", "opus", "m4a", "m4b", "mp4", "mov", "3gp", "3g2", "qt",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Wav,
    Aiff,
    Mpa,
    Adts,
    Flac,
    Ogg,
    Mp4,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Wav => "WAV",
            Format::Aiff => "AIFF",
            Format::Mpa => "MP3",
            Format::Adts => "ADTS",
            Format::Flac => "FLAC",
            Format::Ogg => "Ogg",
            Format::Mp4 => "MP4",
        }
    }
}

fn extension(name_hint: &str) -> String {
    let file = name_hint.rsplit(['/', '\\']).next().unwrap_or("");
    match file.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => String::new(),
    }
}

fn format_for_extension(ext: &str) -> Option<Format> {
    Some(match ext {
        "wav" | "wave" | "rf64" | "bw64" => Format::Wav,
        "aif" | "aiff" | "aifc" => Format::Aiff,
        "mp3" | "mp2" | "mp1" | "mpa" => Format::Mpa,
        "aac" | "adts" => Format::Adts,
        "flac" => Format::Flac,
        "ogg" | "oga" | "opus" => Format::Ogg,
        "m4a" | "m4b" | "mp4" | "mov" | "3gp" | "3g2" | "qt" => Format::Mp4,
        _ => return None,
    })
}

const MP4_TOP_BOXES: &[&[u8; 4]] = &[
    b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"pnot", b"uuid", b"moof", b"styp",
    b"sidx", b"junk", b"PICT",
];

/// Detect the format of `h` (after any ID3v2 tag).
fn detect(h: &[u8], ext: &str) -> Option<Format> {
    if h.len() >= 12
        && (&h[..4] == b"RIFF" || &h[..4] == b"RF64" || &h[..4] == b"BW64")
        && &h[8..12] == b"WAVE"
    {
        return Some(Format::Wav);
    }
    if h.len() >= 12 && &h[..4] == b"FORM" && (&h[8..12] == b"AIFF" || &h[8..12] == b"AIFC") {
        return Some(Format::Aiff);
    }
    if h.starts_with(b"fLaC") {
        return Some(Format::Flac);
    }
    if h.starts_with(b"OggS") {
        return Some(Format::Ogg);
    }
    if h.len() >= 8 && MP4_TOP_BOXES.iter().any(|t| &h[4..8] == *t) {
        let size = io::be32(h, 0);
        if size == 0 || size == 1 || size >= 8 {
            return Some(Format::Mp4);
        }
    }
    // frame-synchronised streams: the first sync that is followed by a consistent frame
    let limit = h.len().min(64 * 1024);
    let mut weak: Option<Format> = None;
    for i in 0..limit.saturating_sub(3) {
        if h[i] != 0xFF || h[i + 1] & 0xE0 != 0xE0 {
            continue;
        }
        let rest = &h[i..];
        if let Some(found) = adts::sniff(rest) {
            if found {
                return Some(Format::Adts);
            }
            weak = weak.or(Some(Format::Adts));
        }
        if let Some(found) = mpa::sniff(rest) {
            if found {
                return Some(Format::Mpa);
            }
            weak = weak.or(Some(Format::Mpa));
        }
    }
    // a single frame that the data ends after: trust it only when the name agrees
    match (weak, format_for_extension(ext)) {
        (Some(w), Some(e)) if e == w => Some(w),
        _ => None,
    }
}

/// Recognise a container from its first bytes (16 KiB or more is best; with a leading ID3v2
/// tag, enough to see past it) and an optional file name, which only breaks ties and resolves
/// streams too short to verify. Returns the [`Demuxer::format_name`] [`open`] would use.
pub fn probe(head: &[u8], name_hint: &str) -> Option<&'static str> {
    let ext = extension(name_hint);
    let off = id3::id3v2_len(head) as usize;
    if off > 0 && off >= head.len() {
        // a tag larger than the probe window: MP3 unless the name says otherwise
        return Some(match format_for_extension(&ext) {
            Some(Format::Flac) => "FLAC",
            Some(Format::Adts) => "ADTS",
            _ => "MP3",
        });
    }
    detect(&head[off.min(head.len())..], &ext).map(Format::name)
}

/// Open a container: sniff the content (`name_hint`, usually the file name, breaks ties) and
/// parse its headers.
pub fn open(reader: Box<dyn ReadSeek>, name_hint: &str) -> Result<Box<dyn Demuxer>> {
    let ext = extension(name_hint);
    let mut src = io::Source::new(reader)?;
    let start = id3::skip_id3v2(&mut src)?;
    src.seek(start);
    let head = src.read_upto(64 * 1024)?;
    let fmt = match detect(&head, &ext) {
        Some(f) => f,
        None => return Err(Error::unsupported("unrecognised audio container")),
    };
    src.seek(0);
    Ok(match fmt {
        Format::Wav => Box::new(wav::WavDemuxer::open(src)?),
        Format::Aiff => Box::new(aiff::AiffDemuxer::open(src)?),
        Format::Mpa => Box::new(mpa::MpaDemuxer::open(src, start)?),
        Format::Adts => Box::new(adts::AdtsDemuxer::open(src, start)?),
        Format::Flac => Box::new(flac::FlacDemuxer::open(src, start)?),
        Format::Ogg => Box::new(ogg::OggDemuxer::open(src)?),
        Format::Mp4 => Box::new(mp4::Mp4Demuxer::open(src)?),
    })
}

/// Open a file with [`open`], using its name as the hint.
pub fn open_path(path: impl AsRef<Path>) -> Result<Box<dyn Demuxer>> {
    let path = path.as_ref();
    let file = std::fs::File::open(path)?;
    open(Box::new(file), &path.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(extension("C:\\a\\b.MP3"), "mp3");
        assert_eq!(extension("x/y.tar.flac"), "flac");
        assert_eq!(extension("noext"), "");
        for e in AUDIO_EXTENSIONS {
            assert!(format_for_extension(e).is_some(), "{e}");
        }
    }

    #[test]
    fn probe_rejects_noise() {
        let noise: Vec<u8> = (0..20_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        assert_eq!(probe(&noise, ""), None);
        assert_eq!(probe(&[], "x.mp3"), None);
    }
}

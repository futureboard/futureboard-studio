//! Ogg Opus (RFC 7845) encoding with libopus, through the `opus` crate.
//!
//! libopus is BSD-3-Clause and is built from source by `opusic-sys` and linked
//! statically; the Ogg container is the pure-Rust `ogg` crate. This replaced
//! MP3 export, whose LAME encoder is LGPL (#284). No FFmpeg/libav, no external
//! process.
//!
//! Stream layout:
//!
//! - Page 1 holds only `OpusHead`: channel count, the encoder's lookahead as
//!   the pre-skip, and the source's sample rate (informational — Opus itself
//!   always runs at 48 kHz).
//! - The next page holds only `OpusTags`: a vendor string plus the export
//!   metadata as Vorbis comments.
//! - Audio follows in 20 ms packets, VBR at the chosen bitrate. A page is
//!   closed about every second so players can seek.
//!
//! Opus encodes at 48 kHz. A source at another rate is converted with
//! `rubato`'s FFT resampler, its filter delay removed, so the 48 kHz stream
//! starts at the source's first sample. The last page's granule position is
//! `pre-skip + source length at 48 kHz`, which trims the padding of the final
//! packet: a conforming decoder returns exactly the source's duration.
//!
//! Samples go in as float, unclipped and undithered; only non-finite values
//! are replaced by silence. Mono and stereo only — more channels would need a
//! channel-mapping-family-1 multistream, which no export path produces.
//!
//! Encoding runs on the export writer thread, never on the audio thread. It
//! reuses its buffers; the only steady-state allocation is one small `Vec`
//! per packet that the Ogg writer keeps until it closes the page.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use rubato::{FftFixedInOut, Resampler};

pub use crate::format::OPUS_SAMPLE_RATE;
use crate::format::{
    AudioEncodeSpec, AudioEncodeSummary, AudioEncoder, AudioFileFormat, AudioSampleFormat,
    EncodeError, OpusEncodeOptions, check_interleaved_len, dither_seed_for, i32_to_f32,
};
use crate::metadata::AudioMetadata;

/// 20 ms at 48 kHz: the packet duration libopus is tuned for.
const FRAME_SAMPLES: usize = 960;

/// libopus's recommended upper bound for one encoded packet.
const MAX_PACKET_BYTES: usize = 4000;

/// Close an Ogg page after this many packets (one second of audio), the same
/// cap libopusenc applies, so seeking has a page boundary at least every
/// second.
const PACKETS_PER_PAGE: u32 = 50;

/// Input frames per resampler call. The FFT size is rounded up from this to a
/// whole number of rate-ratio periods.
const RESAMPLER_CHUNK: usize = 1024;

/// Source rates the resampler is asked to handle.
const MIN_SOURCE_RATE: u32 = 8_000;
const MAX_SOURCE_RATE: u32 = 768_000;

fn opus_error(context: &str, error: opus::Error) -> EncodeError {
    EncodeError::FinalizeFailed(format!("libopus {context}: {error}"))
}

/// Streaming conversion of interleaved audio to 48 kHz, delay removed.
struct ResampleTo48k {
    resampler: FftFixedInOut<f32>,
    channels: usize,
    /// Input frames the resampler takes per call.
    chunk_in: usize,
    /// Interleaved input waiting for a whole chunk.
    pending: Vec<f32>,
    planar_in: Vec<Vec<f32>>,
    planar_out: Vec<Vec<f32>>,
    /// Output frames still to drop: the resampler's filter delay.
    skip: usize,
}

impl ResampleTo48k {
    fn new(source_rate: u32, channels: usize) -> Result<Self, EncodeError> {
        let resampler = FftFixedInOut::<f32>::new(
            source_rate as usize,
            OPUS_SAMPLE_RATE as usize,
            RESAMPLER_CHUNK,
            channels,
        )
        .map_err(|e| {
            EncodeError::UnsupportedSpec(format!(
                "cannot resample {source_rate} Hz to {OPUS_SAMPLE_RATE} Hz for Opus: {e}"
            ))
        })?;
        let chunk_in = resampler.input_frames_next();
        let chunk_out = resampler.output_frames_max();
        let skip = resampler.output_delay();
        Ok(Self {
            channels,
            chunk_in,
            pending: Vec::with_capacity(chunk_in * channels * 2),
            planar_in: (0..channels).map(|_| vec![0.0; chunk_in]).collect(),
            planar_out: (0..channels).map(|_| vec![0.0; chunk_out]).collect(),
            skip,
            resampler,
        })
    }

    /// Append `interleaved` and convert every whole chunk, appending the
    /// 48 kHz result (after the delay) to `out`.
    fn push(&mut self, interleaved: &[f32], out: &mut Vec<f32>) -> Result<(), EncodeError> {
        self.pending.extend_from_slice(interleaved);
        let stride = self.chunk_in * self.channels;
        let mut consumed = 0;
        while self.pending.len() - consumed >= stride {
            let chunk = &self.pending[consumed..consumed + stride];
            for (frame, samples) in chunk.chunks_exact(self.channels).enumerate() {
                for (plane, &sample) in self.planar_in.iter_mut().zip(samples) {
                    plane[frame] = sample;
                }
            }
            consumed += stride;
            let (_, written) = self
                .resampler
                .process_into_buffer(&self.planar_in, &mut self.planar_out, None)
                .map_err(|e| EncodeError::FinalizeFailed(format!("resampler: {e}")))?;
            let dropped = self.skip.min(written);
            self.skip -= dropped;
            out.reserve((written - dropped) * self.channels);
            for frame in dropped..written {
                for plane in &self.planar_out {
                    out.push(plane[frame]);
                }
            }
        }
        self.pending.drain(..consumed);
        Ok(())
    }

    /// Push one chunk of silence: what flushes the tail out of the filter.
    fn push_silence(&mut self, out: &mut Vec<f32>) -> Result<(), EncodeError> {
        let silence = vec![0.0; self.chunk_in * self.channels];
        self.push(&silence, out)
    }
}

pub struct OpusEncoder {
    encoder: opus::Encoder,
    writer: PacketWriter<'static, BufWriter<File>>,
    serial: u32,
    spec: AudioEncodeSpec,
    /// Encoder lookahead at 48 kHz, written as the OpusHead pre-skip.
    pre_skip: u16,
    /// `None` when the source is already 48 kHz.
    resampler: Option<ResampleTo48k>,
    /// Source frames received.
    frames_in: u64,
    /// Frames of 48 kHz signal handed to the packetizer so far.
    signal_48k: u64,
    /// Interleaved 48 kHz audio not yet encoded (less than one packet after
    /// each write).
    pcm: Vec<f32>,
    /// Sanitized input / resampler output scratch.
    scratch: Vec<f32>,
    /// Encoded-packet scratch.
    packet: Vec<u8>,
    /// The newest packet, held back so the last one can be written as the
    /// end of the stream with the trimmed granule position.
    held: Option<Vec<u8>>,
    /// Packets encoded so far, the held one included.
    packets: u64,
    packets_on_page: u32,
    finalized: bool,
}

impl OpusEncoder {
    pub fn create(
        path: impl AsRef<Path>,
        spec: AudioEncodeSpec,
        options: OpusEncodeOptions,
        metadata: AudioMetadata,
    ) -> Result<Self, EncodeError> {
        spec.validate()?;
        let channels = match spec.channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            other => {
                return Err(EncodeError::UnsupportedSpec(format!(
                    "Opus export supports mono or stereo only, got {other} channels"
                )));
            }
        };
        if !(MIN_SOURCE_RATE..=MAX_SOURCE_RATE).contains(&spec.sample_rate) {
            return Err(EncodeError::UnsupportedSpec(format!(
                "Opus export supports {MIN_SOURCE_RATE}..={MAX_SOURCE_RATE} Hz sources, got {}",
                spec.sample_rate
            )));
        }

        let mut encoder = opus::Encoder::new(OPUS_SAMPLE_RATE, channels, opus::Application::Audio)
            .map_err(|e| opus_error("encoder init", e))?;
        let configure = |encoder: &mut opus::Encoder| -> Result<i32, opus::Error> {
            encoder.set_bitrate(opus::Bitrate::Bits(options.bitrate.bits_per_second()))?;
            encoder.set_vbr(true)?;
            encoder.set_vbr_constraint(false)?;
            encoder.set_complexity(i32::from(options.complexity.min(10)))?;
            encoder.set_signal(opus::Signal::Music)?;
            // The render is float; tell the encoder it may use 24 bits of it.
            encoder.set_lsb_depth(24)?;
            encoder.get_lookahead()
        };
        let lookahead = configure(&mut encoder).map_err(|e| opus_error("encoder setup", e))?;
        let pre_skip = u16::try_from(lookahead).map_err(|_| {
            EncodeError::FinalizeFailed(format!("libopus reported lookahead {lookahead}"))
        })?;

        let resampler = (spec.sample_rate != OPUS_SAMPLE_RATE)
            .then(|| ResampleTo48k::new(spec.sample_rate, spec.channels as usize))
            .transpose()?;

        let path = path.as_ref();
        let serial = dither_seed_for(path);
        let mut writer = PacketWriter::new(BufWriter::new(File::create(path)?));
        writer.write_packet(
            opus_head(spec.channels as u8, pre_skip, spec.sample_rate),
            serial,
            PacketWriteEndInfo::EndPage,
            0,
        )?;
        writer.write_packet(opus_tags(&metadata), serial, PacketWriteEndInfo::EndPage, 0)?;

        Ok(Self {
            encoder,
            writer,
            serial,
            spec,
            pre_skip,
            resampler,
            frames_in: 0,
            signal_48k: 0,
            pcm: Vec::with_capacity(FRAME_SAMPLES * 2 * 2),
            scratch: Vec::new(),
            packet: vec![0; MAX_PACKET_BYTES],
            held: None,
            packets: 0,
            packets_on_page: 0,
            finalized: false,
        })
    }

    /// The source length in 48 kHz frames: the duration a decoder returns.
    fn target_48k_frames(&self) -> u64 {
        if self.spec.sample_rate == OPUS_SAMPLE_RATE {
            return self.frames_in;
        }
        let rate = u128::from(self.spec.sample_rate);
        ((u128::from(self.frames_in) * u128::from(OPUS_SAMPLE_RATE) + rate / 2) / rate) as u64
    }

    /// Take the source samples in `self.scratch` through the resampler (if
    /// any) into the packet buffer, then encode every whole packet.
    fn push_scratch(&mut self) -> Result<(), EncodeError> {
        let channels = self.spec.channels as usize;
        self.frames_in += (self.scratch.len() / channels) as u64;
        match self.resampler.as_mut() {
            Some(resampler) => {
                let before = self.pcm.len();
                resampler.push(&self.scratch, &mut self.pcm)?;
                self.signal_48k += ((self.pcm.len() - before) / channels) as u64;
            }
            None => {
                self.pcm.extend_from_slice(&self.scratch);
                self.signal_48k += (self.scratch.len() / channels) as u64;
            }
        }
        self.encode_whole_packets()
    }

    fn encode_whole_packets(&mut self) -> Result<(), EncodeError> {
        let stride = FRAME_SAMPLES * self.spec.channels as usize;
        let mut consumed = 0;
        while self.pcm.len() - consumed >= stride {
            let len = self
                .encoder
                .encode_float(&self.pcm[consumed..consumed + stride], &mut self.packet)
                .map_err(|e| opus_error("encode", e))?;
            consumed += stride;
            self.write_held(false)?;
            self.held = Some(self.packet[..len].to_vec());
            self.packets += 1;
        }
        self.pcm.drain(..consumed);
        Ok(())
    }

    /// Write the held packet. Its granule position is the 48 kHz sample count
    /// at its end; the stream's last packet instead carries the trimmed
    /// position that marks where the source ends.
    fn write_held(&mut self, end_of_stream: bool) -> Result<(), EncodeError> {
        let Some(packet) = self.held.take() else {
            return Ok(());
        };
        let (info, granule) = if end_of_stream {
            (
                PacketWriteEndInfo::EndStream,
                u64::from(self.pre_skip) + self.target_48k_frames(),
            )
        } else {
            self.packets_on_page += 1;
            let info = if self.packets_on_page >= PACKETS_PER_PAGE {
                self.packets_on_page = 0;
                PacketWriteEndInfo::EndPage
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            (info, self.packets * FRAME_SAMPLES as u64)
        };
        self.writer
            .write_packet(packet, self.serial, info, granule)?;
        Ok(())
    }
}

impl AudioEncoder for OpusEncoder {
    fn format(&self) -> AudioFileFormat {
        AudioFileFormat::Opus
    }

    fn spec(&self) -> AudioEncodeSpec {
        self.spec.clone()
    }

    fn write_interleaved_f32(&mut self, frames: &[f32]) -> Result<(), EncodeError> {
        if self.finalized {
            return Err(EncodeError::InvalidInput("write after finalize".into()));
        }
        check_interleaved_len(frames.len(), self.spec.channels)?;
        self.scratch.clear();
        self.scratch
            .extend(frames.iter().map(|&x| if x.is_finite() { x } else { 0.0 }));
        self.push_scratch()
    }

    fn write_interleaved_i32(&mut self, frames: &[i32]) -> Result<(), EncodeError> {
        if self.finalized {
            return Err(EncodeError::InvalidInput("write after finalize".into()));
        }
        check_interleaved_len(frames.len(), self.spec.channels)?;
        self.scratch.clear();
        self.scratch.extend(frames.iter().map(|&x| i32_to_f32(x)));
        self.push_scratch()
    }

    fn finalize(&mut self) -> Result<AudioEncodeSummary, EncodeError> {
        if self.finalized {
            return Err(EncodeError::FinalizeFailed(
                "encoder already finalized".into(),
            ));
        }
        self.finalized = true;
        let channels = self.spec.channels as usize;
        let target = self.target_48k_frames();

        // Flush the resampler's tail, then keep exactly the source length.
        if let Some(resampler) = self.resampler.as_mut() {
            while self.signal_48k < target {
                let before = self.pcm.len();
                resampler.push_silence(&mut self.pcm)?;
                self.signal_48k += ((self.pcm.len() - before) / channels) as u64;
            }
            let excess = (self.signal_48k - target) as usize;
            self.pcm.truncate(self.pcm.len() - excess * channels);
            self.signal_48k = target;
        }

        // Pad with silence so the packets cover the pre-skip plus the whole
        // signal; the trimmed final granule position drops the padding.
        let needed = u64::from(self.pre_skip) + target;
        let total_packets = needed.div_ceil(FRAME_SAMPLES as u64).max(1);
        let padded_frames = total_packets * FRAME_SAMPLES as u64;
        let encoded_frames = self.packets * FRAME_SAMPLES as u64;
        let buffered_frames = (self.pcm.len() / channels) as u64;
        let pad = padded_frames.saturating_sub(encoded_frames + buffered_frames) as usize;
        self.pcm.resize(self.pcm.len() + pad * channels, 0.0);
        self.encode_whole_packets()?;
        debug_assert!(self.pcm.is_empty());
        debug_assert_eq!(self.packets, total_packets);

        self.write_held(true)?;
        let file = self.writer.inner_mut();
        file.flush()?;
        file.get_ref().sync_all()?;
        let bytes_written = file.get_ref().metadata()?.len();

        Ok(AudioEncodeSummary {
            format: AudioFileFormat::Opus,
            // What the file decodes to: always 48 kHz, the source's length.
            sample_rate: OPUS_SAMPLE_RATE,
            channels: self.spec.channels,
            sample_format: AudioSampleFormat::F32,
            frames_written: target,
            bytes_written,
        })
    }
}

/// The 19-byte `OpusHead` identification header (RFC 7845 §5.1), channel
/// mapping family 0 (mono/stereo).
fn opus_head(channels: u8, pre_skip: u16, input_sample_rate: u32) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(channels);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&input_sample_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family
    head
}

/// The `OpusTags` comment header (RFC 7845 §5.2): vendor string plus the
/// metadata as `FIELD=value` Vorbis comments.
fn opus_tags(metadata: &AudioMetadata) -> Vec<u8> {
    let vendor = format!("Futureboard Studio ({})", opus::version());
    let comments = metadata.vorbis_comments();
    let mut tags = Vec::with_capacity(64);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor.as_bytes());
    tags.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (field, value) in comments {
        let length = field.len() + 1 + value.len();
        tags.extend_from_slice(&(length as u32).to_le_bytes());
        tags.extend_from_slice(field.as_bytes());
        tags.push(b'=');
        tags.extend_from_slice(value.as_bytes());
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OpusBitrate;
    use ogg::reading::PacketReader;
    use std::f64::consts::TAU;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "futureboard-{name}-{}-{}.opus",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    fn spec(rate: u32, channels: u16) -> AudioEncodeSpec {
        AudioEncodeSpec {
            sample_rate: rate,
            channels,
            sample_format: AudioSampleFormat::F32,
        }
    }

    /// Per-channel test tone: a different pitch per channel, so a channel
    /// swap would not correlate.
    fn tone(channel: usize, t_seconds: f64) -> f32 {
        let hz = [440.0, 660.0][channel];
        ((TAU * hz * t_seconds).sin() * 0.5) as f32
    }

    fn source(rate: u32, channels: usize, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| (0..channels).map(move |c| tone(c, n as f64 / rate as f64)))
            .collect()
    }

    struct Decoded {
        head: Vec<u8>,
        tags: Vec<u8>,
        /// The interleaved 48 kHz output, pre-skip and end trimming applied.
        pcm: Vec<f32>,
        channels: usize,
        pre_skip: usize,
        audio_packets: usize,
        audio_pages: usize,
    }

    /// Read an Ogg Opus file back the way a player does: check the header
    /// pages, decode every audio packet with libopus, then drop the pre-skip
    /// and trim to the final granule position.
    fn decode(path: &Path) -> Decoded {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[..4], b"OggS", "Ogg capture pattern");
        let mut reader = PacketReader::new(std::io::Cursor::new(bytes));

        let head = reader.read_packet_expected().unwrap();
        assert!(head.first_in_stream(), "OpusHead opens the stream");
        assert!(head.last_in_page(), "OpusHead is alone on its page");
        assert_eq!(head.absgp_page(), 0);
        let tags = reader.read_packet_expected().unwrap();
        assert!(tags.last_in_page(), "OpusTags ends its page");
        assert_eq!(tags.absgp_page(), 0);
        assert_eq!(tags.stream_serial(), head.stream_serial());

        assert_eq!(&head.data[..8], b"OpusHead");
        assert_eq!(head.data.len(), 19);
        let channels = head.data[9] as usize;
        let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;

        let mut decoder = opus::Decoder::new(
            OPUS_SAMPLE_RATE,
            if channels == 1 {
                opus::Channels::Mono
            } else {
                opus::Channels::Stereo
            },
        )
        .unwrap();
        let mut pcm = Vec::new();
        let mut out = vec![0.0f32; 5760 * channels];
        let mut final_granule = None;
        let mut audio_packets = 0;
        let mut audio_pages = 0;
        let mut last_granule = 0;
        while let Some(packet) = reader.read_packet().unwrap() {
            assert!(final_granule.is_none(), "packet after end of stream");
            audio_packets += 1;
            let frames = decoder.decode_float(&packet.data, &mut out, false).unwrap();
            assert_eq!(frames, FRAME_SAMPLES, "20 ms packets");
            pcm.extend_from_slice(&out[..frames * channels]);
            if packet.last_in_page() {
                audio_pages += 1;
                assert!(packet.absgp_page() >= last_granule, "granules increase");
                last_granule = packet.absgp_page();
            }
            if packet.last_in_stream() {
                final_granule = Some(packet.absgp_page());
            }
        }
        let final_granule = final_granule.expect("stream has an end-of-stream page") as usize;
        let length = final_granule - pre_skip;
        assert!(pcm.len() / channels >= final_granule, "granule within data");
        let pcm = pcm[pre_skip * channels..(pre_skip + length) * channels].to_vec();
        Decoded {
            head: head.data,
            tags: tags.data,
            pcm,
            channels,
            pre_skip,
            audio_packets,
            audio_pages,
        }
    }

    /// Normalized correlation of channel `channel` of `decoded` with the
    /// reference tone at 48 kHz, at `lag` samples, skipping the edges.
    fn correlation(decoded: &Decoded, channel: usize, lag: isize) -> f64 {
        let frames = decoded.pcm.len() / decoded.channels;
        let (mut dot, mut a2, mut b2) = (0.0, 0.0, 0.0);
        for n in 2_000..frames.saturating_sub(2_000) {
            let reference = f64::from(tone(channel, (n as isize - lag) as f64 / 48_000.0));
            let sample = f64::from(decoded.pcm[n * decoded.channels + channel]);
            dot += reference * sample;
            a2 += reference * reference;
            b2 += sample * sample;
        }
        dot / (a2.sqrt() * b2.sqrt()).max(1e-12)
    }

    fn encode(path: &Path, rate: u32, channels: u16, frames: usize, block: usize) {
        let mut encoder = OpusEncoder::create(
            path,
            spec(rate, channels),
            OpusEncodeOptions::default(),
            AudioMetadata::default(),
        )
        .unwrap();
        let samples = source(rate, channels as usize, frames);
        // Odd block sizes exercise the packet and resampler buffering.
        for chunk in samples.chunks(block * channels as usize) {
            encoder.write_interleaved_f32(chunk).unwrap();
        }
        let summary = encoder.finalize().unwrap();
        let expected = (frames as u64 * 48_000 + rate as u64 / 2) / rate as u64;
        assert_eq!(summary.sample_rate, 48_000);
        assert_eq!(summary.frames_written, expected);
        assert_eq!(
            summary.bytes_written,
            std::fs::metadata(path).unwrap().len()
        );
    }

    fn assert_round_trip(rate: u32, channels: u16, frames: usize) {
        let path = temp_path(&format!("rt-{rate}-{channels}"));
        encode(&path, rate, channels, frames, 1_003);
        let decoded = decode(&path);

        assert_eq!(decoded.channels, channels as usize);
        let rate_field = u32::from_le_bytes(decoded.head[12..16].try_into().unwrap());
        assert_eq!(rate_field, rate, "OpusHead carries the source rate");
        assert!(decoded.pre_skip > 0, "pre-skip is the encoder lookahead");
        assert_eq!(&decoded.tags[..8], b"OpusTags");

        let expected = (frames as u64 * 48_000 + rate as u64 / 2) / rate as u64;
        assert_eq!(
            (decoded.pcm.len() / decoded.channels) as u64,
            expected,
            "decoded length equals the source length at 48 kHz"
        );
        for channel in 0..channels as usize {
            let at_zero = correlation(&decoded, channel, 0);
            assert!(
                at_zero > 0.98,
                "{rate} Hz channel {channel}: correlation {at_zero}"
            );
            // Timing: no lag fits better than none (delay fully removed).
            let best = (-8..=8)
                .max_by(|&a, &b| {
                    correlation(&decoded, channel, a).total_cmp(&correlation(&decoded, channel, b))
                })
                .unwrap();
            assert!(
                best.abs() <= 1,
                "{rate} Hz channel {channel}: best lag {best}"
            );
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stereo_48k_round_trips_through_libopus() {
        assert_round_trip(48_000, 2, 48_000 + 777);
    }

    #[test]
    fn stereo_44k1_is_resampled_and_round_trips() {
        assert_round_trip(44_100, 2, 44_100 + 777);
    }

    #[test]
    fn mono_and_high_rate_sources_round_trip() {
        assert_round_trip(44_100, 1, 30_000);
        assert_round_trip(96_000, 2, 96_000);
        assert_round_trip(88_200, 2, 50_000);
    }

    #[test]
    fn pages_close_about_every_second() {
        let path = temp_path("pages");
        encode(&path, 48_000, 2, 48_000 * 3, 4_096);
        let decoded = decode(&path);
        // 3 s of 20 ms packets plus padding, 50 per page.
        assert_eq!(decoded.audio_packets, 151);
        assert_eq!(decoded.audio_pages, 4);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn metadata_lands_in_opus_tags() {
        let path = temp_path("tags");
        let metadata = AudioMetadata {
            title: Some("Song".into()),
            artist: Some("Band".into()),
            album: None,
            date: Some("2026-11-09".into()),
            comment: Some("  ".into()),
        };
        let mut encoder = OpusEncoder::create(
            &path,
            spec(48_000, 2),
            OpusEncodeOptions {
                bitrate: OpusBitrate::Kbps96,
                complexity: 5,
            },
            metadata,
        )
        .unwrap();
        encoder
            .write_interleaved_f32(&source(48_000, 2, 4_800))
            .unwrap();
        encoder.finalize().unwrap();

        let tags = decode(&path).tags;
        let vendor_len = u32::from_le_bytes(tags[8..12].try_into().unwrap()) as usize;
        let vendor = std::str::from_utf8(&tags[12..12 + vendor_len]).unwrap();
        assert!(
            vendor.starts_with("Futureboard Studio (libopus"),
            "{vendor}"
        );
        let mut at = 12 + vendor_len;
        let count = u32::from_le_bytes(tags[at..at + 4].try_into().unwrap());
        at += 4;
        let mut comments = Vec::new();
        for _ in 0..count {
            let len = u32::from_le_bytes(tags[at..at + 4].try_into().unwrap()) as usize;
            at += 4;
            comments.push(String::from_utf8(tags[at..at + len].to_vec()).unwrap());
            at += len;
        }
        assert_eq!(at, tags.len());
        // Unset and blank fields are left out.
        assert_eq!(comments, ["TITLE=Song", "ARTIST=Band", "DATE=2026-11-09"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn integer_input_and_an_empty_stream_are_valid() {
        let path = temp_path("i32");
        let mut encoder = OpusEncoder::create(
            &path,
            spec(48_000, 1),
            OpusEncodeOptions::default(),
            AudioMetadata::default(),
        )
        .unwrap();
        let samples: Vec<i32> = (0..9_600)
            .map(|n| ((TAU * 440.0 * n as f64 / 48_000.0).sin() * 0.5 * i32::MAX as f64) as i32)
            .collect();
        encoder.write_interleaved_i32(&samples).unwrap();
        encoder.finalize().unwrap();
        let decoded = decode(&path);
        assert_eq!(decoded.pcm.len(), 9_600);
        assert!(correlation(&decoded, 0, 0) > 0.98);
        let _ = std::fs::remove_file(&path);

        let mut encoder = OpusEncoder::create(
            &path,
            spec(44_100, 2),
            OpusEncodeOptions::default(),
            AudioMetadata::default(),
        )
        .unwrap();
        let summary = encoder.finalize().unwrap();
        assert_eq!(summary.frames_written, 0);
        let decoded = decode(&path);
        assert!(decoded.pcm.is_empty());
        assert_eq!(decoded.audio_packets, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_what_it_cannot_encode() {
        let path = temp_path("reject");
        let result = OpusEncoder::create(
            &path,
            spec(48_000, 6),
            OpusEncodeOptions::default(),
            AudioMetadata::default(),
        );
        assert!(matches!(result, Err(EncodeError::UnsupportedSpec(_))));
        let result = OpusEncoder::create(
            &path,
            spec(4_000, 2),
            OpusEncodeOptions::default(),
            AudioMetadata::default(),
        );
        assert!(matches!(result, Err(EncodeError::UnsupportedSpec(_))));

        let mut encoder = OpusEncoder::create(
            &path,
            spec(48_000, 2),
            OpusEncodeOptions::default(),
            AudioMetadata::default(),
        )
        .unwrap();
        assert!(matches!(
            encoder.write_interleaved_f32(&[0.0; 3]),
            Err(EncodeError::InvalidInput(_))
        ));
        encoder.finalize().unwrap();
        assert!(encoder.finalize().is_err());
        assert!(encoder.write_interleaved_f32(&[0.0; 2]).is_err());
        let _ = std::fs::remove_file(path);
    }
}

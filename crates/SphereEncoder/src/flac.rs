//! FLAC encoding via the pure-Rust `flacenc` crate. No FFmpeg/libav, no native
//! dependencies, no external processes.
//!
//! Streaming: samples are encoded frame by frame as they arrive and written
//! straight to the file, so memory stays bounded however long the render is
//! or however many stems are encoded at once. The STREAMINFO block (total
//! samples, frame sizes, MD5) is written as a placeholder up front and
//! patched in place on [`finalize`](AudioEncoder::finalize). Up to
//! [`MAX_WORKERS`] blocks are encoded in parallel at a time, which keeps the
//! throughput of `flacenc`'s whole-stream parallel encoder. The bytes are
//! identical to what that encoder produces from the same samples.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Frame, StreamInfo};
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

use crate::format::{
    AudioDither, AudioEncodeSpec, AudioEncodeSummary, AudioEncoder, AudioFileFormat,
    AudioSampleFormat, EncodeError, FlacEncodeOptions, Quantizer, check_interleaved_len,
};
use crate::metadata::AudioMetadata;

/// Supported FLAC sample rates (Futureboard's working rates).
const SUPPORTED_RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

/// Most blocks encoded in parallel. Bounds both the threads one encoder uses
/// and the samples it holds (`MAX_WORKERS * BLOCKS_PER_WORKER` blocks).
const MAX_WORKERS: usize = 8;
/// Blocks each worker encodes per batch.
const BLOCKS_PER_WORKER: usize = 2;

/// `fLaC` plus the STREAMINFO block header: last-metadata-block flag, type 0,
/// and its 34-byte length. The flag is cleared when a VORBIS_COMMENT block
/// follows (see [`FlacEncoder::write_stream_header`]).
const STREAMINFO_HEADER: [u8; 8] = [b'f', b'L', b'a', b'C', 0x80, 0x00, 0x00, 0x22];
/// Size of the STREAMINFO block body.
const STREAMINFO_BYTES: u64 = 34;
/// Last-metadata-block flag in a metadata block header's first byte.
const LAST_METADATA_BLOCK: u8 = 0x80;
/// Metadata block type of a VORBIS_COMMENT block.
const VORBIS_COMMENT_BLOCK: u8 = 4;
/// Largest metadata block body the 24-bit length field can describe.
const MAX_METADATA_BLOCK_BYTES: usize = (1 << 24) - 1;
/// Vendor string of the VORBIS_COMMENT block.
const VENDOR: &str = "Futureboard Studio";

/// The VORBIS_COMMENT metadata block (header and body) for `metadata`, always
/// the last metadata block. Empty when there is nothing to write, or when the
/// tags would not fit a metadata block (they are dropped rather than failing
/// the export).
fn vorbis_comment_block(metadata: &AudioMetadata) -> Vec<u8> {
    if metadata.is_empty() {
        return Vec::new();
    }
    let body = metadata.vorbis_comment_body(VENDOR);
    if body.len() > MAX_METADATA_BLOCK_BYTES {
        tracing::warn!(
            "FLAC tags left out: {} bytes do not fit a metadata block",
            body.len()
        );
        return Vec::new();
    }
    let length = (body.len() as u32).to_be_bytes();
    let mut block = Vec::with_capacity(4 + body.len());
    block.push(LAST_METADATA_BLOCK | VORBIS_COMMENT_BLOCK);
    block.extend_from_slice(&length[1..]);
    block.extend_from_slice(&body);
    block
}

pub struct FlacEncoder {
    writer: BufWriter<File>,
    spec: AudioEncodeSpec,
    bits: usize,
    block_size: usize,
    config: Verified<flacenc::config::Encoder>,
    stream_info: StreamInfo,
    /// MD5 and sample count of every sample handed to a frame.
    context: Context,
    /// Interleaved integer samples at `bits` depth not yet encoded: never
    /// more than one batch (`batch_blocks` blocks).
    pending: Vec<i32>,
    /// Blocks encoded per batch.
    batch_blocks: usize,
    /// One frame buffer per worker, reused batch to batch.
    frame_bufs: Vec<FrameBuf>,
    /// Number of the next frame to encode.
    next_frame: usize,
    sink: ByteSink,
    /// Frame bytes written so far (excluding `fLaC` + STREAMINFO).
    frame_bytes: u64,
    quantizer: Quantizer,
    /// The VORBIS_COMMENT block written after STREAMINFO; empty for none.
    comment_block: Vec<u8>,
    finalized: bool,
}

fn flac_error(what: &str, err: impl std::fmt::Debug) -> EncodeError {
    EncodeError::FinalizeFailed(format!("FLAC {what} failed: {err:?}"))
}

impl FlacEncoder {
    pub fn create(
        path: impl AsRef<Path>,
        spec: AudioEncodeSpec,
        options: FlacEncodeOptions,
        metadata: AudioMetadata,
    ) -> Result<Self, EncodeError> {
        spec.validate()?;
        if !SUPPORTED_RATES.contains(&spec.sample_rate) {
            return Err(EncodeError::UnsupportedSpec(format!(
                "FLAC sample rate {} not supported (expected one of {:?})",
                spec.sample_rate, SUPPORTED_RATES
            )));
        }
        let bits = match spec.sample_format {
            AudioSampleFormat::I16 => 16,
            AudioSampleFormat::I24 => 24,
            AudioSampleFormat::I32 | AudioSampleFormat::F32 => {
                return Err(EncodeError::UnsupportedSpec(
                    "FLAC export supports 16-bit or 24-bit integer depth only".to_string(),
                ));
            }
        };
        let block_size = if options.block_size == 0 {
            4096
        } else {
            options.block_size.clamp(64, 32_768)
        };
        let channels = spec.channels as usize;

        let mut config = flacenc::config::Encoder::default();
        config.block_size = block_size;
        let workers = if config.multithread {
            std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .clamp(1, MAX_WORKERS)
        } else {
            1
        };
        let config = config.into_verified().map_err(|(_, err)| {
            EncodeError::UnsupportedSpec(format!("invalid FLAC encoder config: {err}"))
        })?;
        let mut stream_info = StreamInfo::new(spec.sample_rate as usize, channels, bits)
            .map_err(|err| EncodeError::UnsupportedSpec(format!("invalid FLAC stream: {err}")))?;
        // As `flacenc` does: a stream of one short frame still reports
        // `block_size` as its maximum block size.
        stream_info
            .set_block_sizes(block_size, block_size)
            .map_err(|err| EncodeError::UnsupportedSpec(format!("invalid FLAC block: {err}")))?;
        let frame_bufs = (0..workers)
            .map(|_| FrameBuf::with_size(channels, block_size))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| EncodeError::UnsupportedSpec(format!("invalid FLAC block: {err}")))?;
        let batch_blocks = workers * BLOCKS_PER_WORKER;

        let mut encoder = Self {
            writer: BufWriter::new(File::create(path.as_ref())?),
            spec,
            bits,
            block_size,
            config,
            stream_info,
            context: Context::new(bits, channels),
            pending: Vec::with_capacity(batch_blocks * block_size * channels),
            batch_blocks,
            frame_bufs,
            next_frame: 0,
            sink: ByteSink::new(),
            frame_bytes: 0,
            quantizer: Quantizer::for_file(path.as_ref()),
            comment_block: vorbis_comment_block(&metadata),
            finalized: false,
        };
        // Placeholder STREAMINFO, patched on finalize. The tags follow it and
        // are final from the start.
        encoder.write_stream_header()?;
        encoder.writer.write_all(&encoder.comment_block)?;
        Ok(encoder)
    }

    /// Dither the 16/24-bit samples with `dither`, seeded for this file.
    pub fn with_dither(mut self, dither: AudioDither) -> Self {
        self.quantizer = self.quantizer.with_dither(dither);
        self
    }

    fn ensure_open(&self) -> Result<(), EncodeError> {
        if self.finalized {
            return Err(EncodeError::InvalidInput(
                "write after finalize".to_string(),
            ));
        }
        Ok(())
    }

    fn batch_len(&self) -> usize {
        self.batch_blocks * self.block_size * self.spec.channels as usize
    }

    /// `fLaC` and the STREAMINFO block as `stream_info` stands, at the
    /// writer's current position. STREAMINFO is the last metadata block
    /// unless a VORBIS_COMMENT block follows it.
    fn write_stream_header(&mut self) -> Result<(), EncodeError> {
        self.sink.clear();
        self.stream_info
            .write(&mut self.sink)
            .map_err(|err| flac_error("STREAMINFO serialize", err))?;
        let mut header = STREAMINFO_HEADER;
        if !self.comment_block.is_empty() {
            header[4] &= !LAST_METADATA_BLOCK;
        }
        self.writer.write_all(&header)?;
        self.writer.write_all(self.sink.as_slice())?;
        Ok(())
    }

    /// Convert `samples` with `convert`, encoding each batch as it fills, so
    /// at most one batch is ever held whatever the caller's block size.
    fn push_samples<T: Copy>(
        &mut self,
        samples: &[T],
        convert: impl Fn(&mut Quantizer, usize, T) -> i32,
    ) -> Result<(), EncodeError> {
        let batch_len = self.batch_len();
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (batch_len - self.pending.len()).min(rest.len());
            let (now, later) = rest.split_at(take);
            for &x in now {
                let v = convert(&mut self.quantizer, self.bits, x);
                self.pending.push(v);
            }
            rest = later;
            if self.pending.len() == batch_len {
                self.encode_pending()?;
            }
        }
        Ok(())
    }

    /// Encode everything pending (whole blocks, plus a last short block on
    /// finalize) and write the frames in order.
    fn encode_pending(&mut self) -> Result<(), EncodeError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let block_len = self.block_size * self.spec.channels as usize;
        let blocks: Vec<&[i32]> = self.pending.chunks(block_len).collect();
        // MD5 and sample count follow the stream order.
        for block in &blocks {
            self.context
                .fill_interleaved(block)
                .map_err(|err| flac_error("read", err))?;
        }
        let frames = encode_blocks(
            &self.config,
            &blocks,
            self.next_frame,
            &self.stream_info,
            &mut self.frame_bufs,
        )?;
        self.next_frame += blocks.len();
        for frame in frames {
            self.stream_info.update_frame_info(&frame);
            self.sink.clear();
            frame
                .write(&mut self.sink)
                .map_err(|err| flac_error("frame serialize", err))?;
            let bytes = self.sink.as_slice();
            self.writer.write_all(bytes)?;
            self.frame_bytes += bytes.len() as u64;
        }
        self.pending.clear();
        Ok(())
    }
}

/// Encode `blocks` (frame numbers from `first`) on up to `frame_bufs.len()`
/// threads, returning the frames in block order.
fn encode_blocks(
    config: &Verified<flacenc::config::Encoder>,
    blocks: &[&[i32]],
    first: usize,
    stream_info: &StreamInfo,
    frame_bufs: &mut [FrameBuf],
) -> Result<Vec<Frame>, EncodeError> {
    let workers = frame_bufs.len().min(blocks.len()).max(1);
    let encode_some = |worker: usize, frame_buf: &mut FrameBuf| {
        let mut out = Vec::with_capacity(blocks.len().div_ceil(workers));
        for index in (worker..blocks.len()).step_by(workers) {
            frame_buf
                .fill_interleaved(blocks[index])
                .map_err(|err| flac_error("read", err))?;
            let frame =
                flacenc::encode_fixed_size_frame(config, frame_buf, first + index, stream_info)
                    .map_err(|err| flac_error("encode", err))?;
            out.push(frame);
        }
        Ok::<_, EncodeError>(out)
    };
    let per_worker: Vec<Vec<Frame>> = if workers == 1 {
        vec![encode_some(0, &mut frame_bufs[0])?]
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = frame_bufs[..workers]
                .iter_mut()
                .enumerate()
                .map(|(worker, frame_buf)| scope.spawn(move || encode_some(worker, frame_buf)))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| flac_error("encode", "worker panicked"))?
                })
                .collect::<Result<Vec<_>, _>>()
        })?
    };
    // Worker `w` holds blocks `w, w + workers, ...`: interleave them back.
    let mut per_worker: Vec<_> = per_worker.into_iter().map(Vec::into_iter).collect();
    let mut frames = Vec::with_capacity(blocks.len());
    for index in 0..blocks.len() {
        let frame = per_worker[index % workers]
            .next()
            .ok_or_else(|| flac_error("encode", "missing frame"))?;
        frames.push(frame);
    }
    Ok(frames)
}

impl AudioEncoder for FlacEncoder {
    fn format(&self) -> AudioFileFormat {
        AudioFileFormat::Flac
    }

    fn spec(&self) -> AudioEncodeSpec {
        self.spec.clone()
    }

    fn write_interleaved_f32(&mut self, frames: &[f32]) -> Result<(), EncodeError> {
        self.ensure_open()?;
        check_interleaved_len(frames.len(), self.spec.channels)?;
        self.push_samples(frames, |quantizer, bits, x| {
            if bits == 16 {
                quantizer.f32_to_i16(x) as i32
            } else {
                quantizer.f32_to_i24(x)
            }
        })
    }

    fn write_interleaved_i32(&mut self, frames: &[i32]) -> Result<(), EncodeError> {
        self.ensure_open()?;
        check_interleaved_len(frames.len(), self.spec.channels)?;
        // Input is full-scale signed 32-bit; reduce to target depth.
        self.push_samples(frames, |quantizer, bits, x| {
            if bits == 16 {
                quantizer.i32_to_i16(x) as i32
            } else {
                quantizer.i32_to_i24(x)
            }
        })
    }

    fn finalize(&mut self) -> Result<AudioEncodeSummary, EncodeError> {
        if self.finalized {
            return Err(EncodeError::FinalizeFailed(
                "encoder already finalized".to_string(),
            ));
        }
        self.finalized = true;

        self.encode_pending()?;
        let total_samples = self.context.total_samples();
        self.stream_info.set_md5_digest(&self.context.md5_digest());
        self.stream_info.set_total_samples(total_samples);
        // `update_frame_info` folded the last, shorter block into the minimum
        // block size. The format excludes the last block from it, and
        // decoders (Studio's own among them) read min != max as a
        // variable-block-size stream and refuse it — or, for a last block
        // under 16 samples, an invalid one (#352). Every block but the last
        // is `block_size`, so both bounds are `block_size`, as libFLAC writes.
        self.stream_info
            .set_block_sizes(self.block_size, self.block_size)
            .map_err(|err| flac_error("STREAMINFO block size", err))?;

        // Patch the placeholder STREAMINFO in place; the tags after it stay.
        self.writer.flush()?;
        self.writer.seek(SeekFrom::Start(0))?;
        self.write_stream_header()?;
        self.writer.flush()?;
        self.writer.get_mut().sync_all()?;

        Ok(AudioEncodeSummary {
            format: AudioFileFormat::Flac,
            sample_rate: self.spec.sample_rate,
            channels: self.spec.channels,
            sample_format: self.spec.sample_format,
            frames_written: total_samples as u64,
            bytes_written: STREAMINFO_HEADER.len() as u64
                + STREAMINFO_BYTES
                + self.comment_block.len() as u64
                + self.frame_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "futureboard-{name}-{}-{}.flac",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    fn spec(rate: u32, ch: u16, fmt: AudioSampleFormat) -> AudioEncodeSpec {
        AudioEncodeSpec {
            sample_rate: rate,
            channels: ch,
            sample_format: fmt,
        }
    }

    #[test]
    fn encodes_stereo_48k_16bit_sine_with_flac_magic() {
        let path = temp_path("sine");
        let mut enc = FlacEncoder::create(
            &path,
            spec(48_000, 2, AudioSampleFormat::I16),
            FlacEncodeOptions::default(),
            AudioMetadata::default(),
        )
        .unwrap();

        // One second of a 440 Hz sine, stereo.
        let mut block = Vec::new();
        for n in 0..48_000u32 {
            let t = n as f32 / 48_000.0;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            block.push(s);
            block.push(s);
        }
        enc.write_interleaved_f32(&block).unwrap();
        let summary = enc.finalize().unwrap();
        assert_eq!(summary.frames_written, 48_000);
        assert!(summary.bytes_written > 0);

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"fLaC", "missing FLAC stream marker");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_unsupported_sample_format() {
        let path = temp_path("badfmt");
        let result = FlacEncoder::create(
            &path,
            spec(48_000, 2, AudioSampleFormat::F32),
            FlacEncodeOptions::default(),
            AudioMetadata::default(),
        );
        assert!(matches!(result, Err(EncodeError::UnsupportedSpec(_))));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn empty_input_finalizes_without_panic() {
        let path = temp_path("empty");
        let mut enc = FlacEncoder::create(
            &path,
            spec(44_100, 1, AudioSampleFormat::I24),
            FlacEncodeOptions::default(),
            AudioMetadata::default(),
        )
        .unwrap();
        let summary = enc.finalize().unwrap();
        assert_eq!(summary.frames_written, 0);
        let _ = std::fs::remove_file(path);
    }

    /// What the old whole-render path wrote: `flacenc`'s own encoder over
    /// every sample at once.
    fn whole_stream_flac(
        samples: &[i32],
        channels: usize,
        bits: usize,
        block_size: usize,
    ) -> Vec<u8> {
        let mut config = flacenc::config::Encoder::default();
        config.block_size = block_size;
        let config = config.into_verified().map_err(|(_, err)| err).unwrap();
        let source = flacenc::source::MemSource::from_samples(samples, channels, bits, 48_000);
        let stream = flacenc::encode_with_fixed_block_size(&config, source, block_size).unwrap();
        let mut sink = ByteSink::new();
        stream.write(&mut sink).unwrap();
        sink.as_slice().to_vec()
    }

    /// A decaying two-tone signal with a quiet tail.
    fn signal(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels)
            .map(|i| {
                let (n, ch) = ((i / channels) as f32, (i % channels) as f32);
                let t = n / 48_000.0;
                let env = (-t * 3.0).exp();
                env * (0.6 * (t * 2_765.0 + ch).sin() + 0.2 * (t * 9_001.0).sin())
            })
            .collect()
    }

    fn encode_streaming(
        path: &Path,
        fmt: AudioSampleFormat,
        channels: u16,
        block_size: usize,
        dither: AudioDither,
        input: &[f32],
        write_frames: usize,
    ) -> FlacEncoder {
        let mut enc = FlacEncoder::create(
            path,
            spec(48_000, channels, fmt),
            FlacEncodeOptions {
                block_size,
                ..FlacEncodeOptions::default()
            },
            AudioMetadata::default(),
        )
        .unwrap()
        .with_dither(dither);
        for chunk in input.chunks(write_frames * channels as usize) {
            enc.write_interleaved_f32(chunk).unwrap();
            assert!(
                enc.pending.len() <= enc.batch_len(),
                "the encoder held more than one batch"
            );
        }
        enc
    }

    /// #302: the streaming encoder writes, batch by batch, exactly the bytes
    /// `flacenc` produces from the whole render held in memory — including a
    /// short last block, several batches, and writes that straddle blocks.
    #[test]
    fn streaming_output_is_identical_to_the_whole_stream_encoder() {
        for (fmt, bits, channels, frames) in [
            (AudioSampleFormat::I16, 16, 2u16, 20_037usize),
            (AudioSampleFormat::I24, 24, 2, 9_001),
            (AudioSampleFormat::I24, 24, 1, 300),
            (AudioSampleFormat::I16, 16, 6, 5_000),
        ] {
            let input = signal(frames, channels as usize);
            let path = temp_path("stream-identical");
            let mut enc =
                encode_streaming(&path, fmt, channels, 256, AudioDither::Off, &input, 333);
            let summary = enc.finalize().unwrap();
            assert_eq!(summary.frames_written, frames as u64);

            let mut quantizer = Quantizer::new(AudioDither::Off);
            let ints: Vec<i32> = input
                .iter()
                .map(|&x| {
                    if bits == 16 {
                        i32::from(quantizer.f32_to_i16(x))
                    } else {
                        quantizer.f32_to_i24(x)
                    }
                })
                .collect();
            let mut expected = whole_stream_flac(&ints, channels as usize, bits, 256);
            // The one intended difference (#352): `flacenc` records the short
            // last block as the minimum block size; the format excludes the
            // last block, so the streaming encoder writes `block_size`.
            expected[8..10].copy_from_slice(&256u16.to_be_bytes());
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(summary.bytes_written, bytes.len() as u64);
            assert!(
                bytes == expected,
                "{fmt:?} x{channels}: streamed FLAC differs from the whole-stream encoding"
            );
            let _ = std::fs::remove_file(path);
        }
    }

    /// #302: memory does not grow with the render: however much is written,
    /// the encoder holds at most one batch of samples, never the render.
    #[test]
    fn a_long_render_holds_at_most_one_batch() {
        let path = temp_path("bounded");
        let input = signal(4_096, 2);
        let mut enc = FlacEncoder::create(
            &path,
            spec(48_000, 2, AudioSampleFormat::I24),
            FlacEncodeOptions::default(),
            AudioMetadata::default(),
        )
        .unwrap();
        let capacity = enc.pending.capacity();
        assert_eq!(capacity, enc.batch_len());
        // ~68 s of stereo 48 kHz, written in export-sized blocks.
        for _ in 0..800 {
            enc.write_interleaved_f32(&input).unwrap();
            assert!(enc.pending.len() <= enc.batch_len());
        }
        assert_eq!(enc.pending.capacity(), capacity, "the buffer never grew");
        let summary = enc.finalize().unwrap();
        assert_eq!(summary.frames_written, 800 * 4_096);
        let _ = std::fs::remove_file(path);
    }

    /// #352: STREAMINFO's minimum block size excludes the last block, so a
    /// render whose length is not a multiple of the block size still reads
    /// as a fixed-block-size stream (min == max), even when the last block
    /// is shorter than the 16-sample minimum a block size may declare.
    #[test]
    fn short_last_block_keeps_a_fixed_block_size_streaminfo() {
        for frames in [4_096 * 3 + 1, 4_096 * 2 + 1_000, 4_096, 300] {
            let path = temp_path("short-last-block");
            let input = signal(frames, 2);
            let mut enc = encode_streaming(
                &path,
                AudioSampleFormat::I16,
                2,
                4_096,
                AudioDither::Off,
                &input,
                1_024,
            );
            let summary = enc.finalize().unwrap();
            assert_eq!(summary.frames_written, frames as u64);
            let bytes = std::fs::read(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            let min_block = u16::from_be_bytes([bytes[8], bytes[9]]);
            let max_block = u16::from_be_bytes([bytes[10], bytes[11]]);
            assert_eq!(
                (min_block, max_block),
                (4_096, 4_096),
                "{frames} frames: STREAMINFO block sizes"
            );
            let total = (u64::from(bytes[21] & 0x0F) << 32)
                | u64::from(u32::from_be_bytes(bytes[22..26].try_into().unwrap()));
            assert_eq!(total, frames as u64);
        }
    }

    /// #322: dither is deterministic per file (the same render encodes to the
    /// same file) and does change a quiet signal's samples. #362: another
    /// file gets other noise.
    #[test]
    fn dithered_flac_is_reproducible_and_differs_from_undithered() {
        let input: Vec<f32> = signal(6_000, 2).iter().map(|x| x * 1e-4).collect();
        let (path, other) = (temp_path("dither"), temp_path("dither-other"));
        let encode = |path: &Path, dither| {
            let mut enc =
                encode_streaming(path, AudioSampleFormat::I16, 2, 4096, dither, &input, 512);
            enc.finalize().unwrap();
            let bytes = std::fs::read(path).unwrap();
            let _ = std::fs::remove_file(path);
            bytes
        };
        let dithered = encode(&path, AudioDither::Tpdf);
        assert_eq!(
            dithered,
            encode(&path, AudioDither::Tpdf),
            "dither must be seeded"
        );
        assert_ne!(dithered, encode(&path, AudioDither::Off));
        assert_ne!(
            dithered,
            encode(&other, AudioDither::Tpdf),
            "each file gets its own dither"
        );
    }

    /// Tags land in a VORBIS_COMMENT block right after STREAMINFO, which then
    /// stops being the last metadata block; the frames follow the tags, the
    /// header patch on finalize leaves them in place, and the first frame
    /// starts where the last block ends. Without tags STREAMINFO stays last.
    #[test]
    fn tags_are_a_vorbis_comment_block_after_streaminfo() {
        let path = temp_path("tags");
        let metadata = AudioMetadata {
            title: Some("Song".into()),
            artist: Some("Band".into()),
            genre: Some("Ambient".into()),
            isrc: Some("USRC17607839".into()),
            comment: Some("  ".into()),
            ..AudioMetadata::default()
        };
        let mut enc = FlacEncoder::create(
            &path,
            spec(48_000, 2, AudioSampleFormat::I16),
            FlacEncodeOptions::default(),
            metadata,
        )
        .unwrap();
        enc.write_interleaved_f32(&signal(5_000, 2)).unwrap();
        let summary = enc.finalize().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(summary.bytes_written, bytes.len() as u64);

        assert_eq!(&bytes[0..4], b"fLaC");
        assert_eq!(bytes[4], 0x00, "STREAMINFO, not last");
        assert_eq!(&bytes[5..8], &[0, 0, 34]);
        let total = (u64::from(bytes[21] & 0x0F) << 32)
            | u64::from(u32::from_be_bytes(bytes[22..26].try_into().unwrap()));
        assert_eq!(total, 5_000, "STREAMINFO was patched");

        let at = 8 + 34;
        assert_eq!(bytes[at], 0x84, "VORBIS_COMMENT, last");
        let length = u32::from_be_bytes([0, bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        let body = &bytes[at + 4..at + 4 + length];
        let u32_at = |at: usize| u32::from_le_bytes(body[at..at + 4].try_into().unwrap()) as usize;
        let vendor_len = u32_at(0);
        assert_eq!(&body[4..4 + vendor_len], VENDOR.as_bytes());
        let mut cursor = 4 + vendor_len;
        let count = u32_at(cursor);
        cursor += 4;
        let mut comments = Vec::new();
        for _ in 0..count {
            let len = u32_at(cursor);
            cursor += 4;
            comments.push(std::str::from_utf8(&body[cursor..cursor + len]).unwrap());
            cursor += len;
        }
        assert_eq!(cursor, body.len());
        assert_eq!(
            comments,
            [
                "TITLE=Song",
                "ARTIST=Band",
                "GENRE=Ambient",
                "ISRC=USRC17607839"
            ]
        );
        // The first frame (fixed-blocksize sync) follows the last block.
        let frames = at + 4 + length;
        assert_eq!(&bytes[frames..frames + 2], &[0xff, 0xf8]);

        // Blank tags only: STREAMINFO is the only, and last, block.
        let path = temp_path("no-tags");
        let mut enc = FlacEncoder::create(
            &path,
            spec(48_000, 2, AudioSampleFormat::I16),
            FlacEncodeOptions::default(),
            AudioMetadata {
                title: Some(" ".into()),
                ..AudioMetadata::default()
            },
        )
        .unwrap();
        enc.write_interleaved_f32(&signal(500, 2)).unwrap();
        enc.finalize().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes[4], 0x80);
        assert_eq!(&bytes[42..44], &[0xff, 0xf8]);
    }

    /// LiveStage's recorder builds its options as `..AudioEncodeOptions::
    /// default()`: a 24-bit FLAC take through the factory must not be
    /// dithered, i.e. identical to an explicit `AudioDither::Off` encode.
    #[test]
    fn recorder_style_default_options_do_not_dither() {
        let input: Vec<f32> = signal(6_000, 2).iter().map(|x| x * 1e-6).collect();
        let encode = |options: crate::format::AudioEncodeOptions| {
            let path = temp_path("recorder-default");
            let mut enc = crate::format::create_encoder(
                &path,
                spec(48_000, 2, AudioSampleFormat::I24),
                options,
            )
            .unwrap();
            enc.write_interleaved_f32(&input).unwrap();
            enc.finalize().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let _ = std::fs::remove_file(path);
            bytes
        };
        let flac = |dither| crate::format::AudioEncodeOptions {
            format: AudioFileFormat::Flac,
            dither,
            ..crate::format::AudioEncodeOptions::default()
        };
        let recorder = encode(crate::format::AudioEncodeOptions {
            format: AudioFileFormat::Flac,
            ..crate::format::AudioEncodeOptions::default()
        });
        assert_eq!(recorder, encode(flac(AudioDither::Off)));
        assert_ne!(recorder, encode(flac(AudioDither::Tpdf)));
    }
}

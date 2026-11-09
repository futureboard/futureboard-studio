//! Disk-streaming clip source (Phase F).
//!
//! Large compressed files (MP3 / FLAC / OGG) are too big to fully decode into
//! memory, and unlike PCM WAV they cannot be memory-mapped and read directly.
//! This module streams them: a background decoder thread fills a bounded,
//! preallocated ring buffer ahead of the playhead, and the audio callback reads
//! finished stereo frames from the ring with no allocation, no locks, and no
//! file I/O (see `tasks/native/audio-system-spec.md` §7 and the Phase F plan in
//! `audio-system-checklist.md`).
//!
//! ## Realtime safety
//!
//! The ring stores interleaved stereo samples as `AtomicU32` (f32 bit-casts).
//! Per-sample atomic load/store means there is **no data-race UB** even if the
//! producer/consumer window bookkeeping has an edge case — the worst case is a
//! momentarily stale sample, which the window check below already rejects as an
//! underrun. The callback only does relaxed atomic loads; the decoder thread
//! does the decode, seeks, and throttling.
//!
//! ## Window model (single-producer / single-consumer)
//!
//! * `window` — the frames `[start, end)` the ring really holds, packed into
//!   one atomic so the reader always sees a consistent pair (Release by the
//!   worker). Its length never exceeds the ring capacity.
//! * `reader_frame` — last `floor(pos)` the callback asked for (Release by
//!   callback).
//!
//! The window is explicit rather than derived as `[end - capacity, end)`: after
//! a seek the slots below the new start still hold audio from wherever the
//! stream was before, and deriving the start from the end used to serve that
//! audio as valid — a reversed clip, or a short step back past a reposition,
//! played stale samples with no underrun counted.
//!
//! The worker keeps the window filled in the direction the reader is moving.
//! Forward, it decodes ahead of the reader up to `capacity - margin` frames.
//! Backward (a reversed clip), it fills chunks *below* the window, dropping the
//! top of the window first so it never overwrites a slot the reader may still
//! read. In both directions a slot is only rewritten after the window stopped
//! covering it, and the reader keeps at least `THROTTLE_MARGIN` frames away
//! from the edge being dropped.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use std::fs::File;
use std::io;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::audio_file::probe_audio_file;

/// Ring capacity in stereo frames. 262144 frames ≈ 5.46 s @ 48 kHz and uses
/// ~2 MB (`capacity * 2 * 4` bytes) regardless of how long the source file is.
const RING_FRAMES: usize = 1 << 18;

/// Leave this many frames of head-room so the worker never writes a slot the
/// reader (which samples `idx` and `idx + 1`) could still be reading.
const THROTTLE_MARGIN: u64 = 8192;

/// Frames decoded per backward fill. Each fill is one seek plus a forward
/// decode of this many frames into the slots just below the window.
const REVERSE_CHUNK: u64 = 1 << 16;

/// Net reader motion (frames) that flips the worker's fill direction. Small
/// back-and-forth reads (the phase-vocoder grain pairs) cancel out under it.
const DIRECTION_HYSTERESIS: i64 = 2048;

/// A reader move larger than this between two worker polls is a seek, not
/// playback motion, and says nothing about direction.
const DIRECTION_JUMP: u64 = 16384;

/// Bits of the packed window word that hold its length; the start takes the
/// rest (2^44 frames — far beyond any file).
const WINDOW_LEN_BITS: u32 = 20;
const WINDOW_LEN_MASK: u64 = (1 << WINDOW_LEN_BITS) - 1;

#[inline]
fn pack_window(start: u64, end: u64) -> u64 {
    debug_assert!(end >= start && end - start <= WINDOW_LEN_MASK);
    (start << WINDOW_LEN_BITS) | (end - start)
}

#[inline]
fn unpack_window(word: u64) -> (u64, u64) {
    let start = word >> WINDOW_LEN_BITS;
    (start, start + (word & WINDOW_LEN_MASK))
}

/// `FUTUREBOARD_DISK_STREAM_DEBUG=1` enables decoder-thread traces. Cached on
/// first read; never touched from the audio callback.
fn disk_stream_debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_DISK_STREAM_DEBUG").is_some())
}

/// Process-wide disk-underrun counter, surfaced in diagnostics. A streaming
/// read that finds its frame outside the buffered window bumps this and returns
/// silence.
static GLOBAL_READS: AtomicU64 = AtomicU64::new(0);
static GLOBAL_HITS: AtomicU64 = AtomicU64::new(0);
static GLOBAL_UNDERRUNS: AtomicU64 = AtomicU64::new(0);
static GLOBAL_BLOCKS_DECODED: AtomicU64 = AtomicU64::new(0);
static GLOBAL_FRAMES_DECODED: AtomicU64 = AtomicU64::new(0);
static GLOBAL_ACTIVE_SOURCES: AtomicU64 = AtomicU64::new(0);

/// Aggregate process-wide disk stream diagnostics. These are lightweight
/// atomics updated by the streaming reader and worker; the audio callback never
/// logs or blocks to produce them.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiskStreamDiagnostics {
    pub active_sources: u64,
    pub cache_reads: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub underruns: u64,
    pub blocks_decoded: u64,
    pub frames_decoded: u64,
    pub cache_memory_used_bytes: u64,
    pub cache_memory_budget_bytes: u64,
}

pub fn diagnostics() -> DiskStreamDiagnostics {
    let reads = GLOBAL_READS.load(Ordering::Relaxed);
    let hits = GLOBAL_HITS.load(Ordering::Relaxed);
    let underruns = GLOBAL_UNDERRUNS.load(Ordering::Relaxed);
    let active_sources = GLOBAL_ACTIVE_SOURCES.load(Ordering::Relaxed);
    let source_bytes = RING_FRAMES as u64 * 2 * std::mem::size_of::<f32>() as u64;
    DiskStreamDiagnostics {
        active_sources,
        cache_reads: reads,
        cache_hits: hits,
        cache_misses: reads.saturating_sub(hits),
        underruns,
        blocks_decoded: GLOBAL_BLOCKS_DECODED.load(Ordering::Relaxed),
        frames_decoded: GLOBAL_FRAMES_DECODED.load(Ordering::Relaxed),
        // StreamingSource uses one bounded stereo f32 ring per active source.
        cache_memory_used_bytes: active_sources.saturating_mul(source_bytes),
        cache_memory_budget_bytes: active_sources.saturating_mul(source_bytes),
    }
}

/// Lock-free stereo ring shared between the decoder thread (producer) and the
/// audio callback (consumer).
#[derive(Debug)]
pub struct StreamingRing {
    capacity: usize,
    total_frames: u64,
    sample_rate: u32,
    /// Interleaved stereo f32 bits; length `capacity * 2`.
    data: Box<[AtomicU32]>,
    /// Packed `[start, end)` of the frames the ring holds (see module docs).
    window: AtomicU64,
    reader_frame: AtomicU64,
    underruns: AtomicU64,
}

impl StreamingRing {
    fn new(total_frames: u64, sample_rate: u32, capacity: usize) -> Self {
        let capacity = capacity.clamp(2, WINDOW_LEN_MASK as usize);
        let data = (0..capacity * 2)
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            capacity,
            total_frames,
            sample_rate,
            data,
            window: AtomicU64::new(pack_window(0, 0)),
            reader_frame: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
        }
    }

    /// The published window `[start, end)`.
    #[inline]
    fn window(&self) -> (u64, u64) {
        unpack_window(self.window.load(Ordering::Acquire))
    }

    #[inline]
    fn reader_frame(&self) -> u64 {
        self.reader_frame.load(Ordering::Acquire)
    }

    #[inline]
    fn load_frame(&self, idx: u64) -> (f32, f32) {
        let slot = (idx as usize % self.capacity) * 2;
        (
            f32::from_bits(self.data[slot].load(Ordering::Relaxed)),
            f32::from_bits(self.data[slot + 1].load(Ordering::Relaxed)),
        )
    }

    // ── Producer side (decoder thread) ───────────────────────────────────────

    #[inline]
    fn store_frame(&self, frame: u64, l: f32, r: f32) {
        let slot = (frame as usize % self.capacity) * 2;
        self.data[slot].store(l.to_bits(), Ordering::Relaxed);
        self.data[slot + 1].store(r.to_bits(), Ordering::Relaxed);
    }

    /// Publish the frames `[start, end)` as readable. Shrink the window before
    /// rewriting the slots it drops; grow it only after the new frames are
    /// stored.
    #[inline]
    fn publish_window(&self, start: u64, end: u64) {
        debug_assert!(end >= start && end - start <= self.capacity as u64);
        self.window
            .store(pack_window(start, end), Ordering::Release);
    }

    // ── Consumer side (audio callback) ───────────────────────────────────────

    /// Read a linearly-interpolated stereo sample at fractional frame `pos`.
    /// Realtime-safe: relaxed atomic loads only. Out-of-window reads return
    /// silence and bump the underrun counters.
    #[inline]
    pub fn read_interp(&self, pos: f64) -> (f32, f32) {
        GLOBAL_READS.fetch_add(1, Ordering::Relaxed);
        if pos < 0.0 || self.total_frames == 0 {
            return (0.0, 0.0);
        }
        let idx = pos.floor() as u64;
        if idx >= self.total_frames {
            return (0.0, 0.0);
        }
        // Tell the worker where playback is so it can prefetch / reposition.
        self.reader_frame.store(idx, Ordering::Release);

        let (start, end) = self.window();
        if idx < start || idx >= end {
            self.bump_underrun();
            return (0.0, 0.0);
        }
        GLOBAL_HITS.fetch_add(1, Ordering::Relaxed);

        // Next frame for interpolation, clamped to the last real frame.
        let next = (idx + 1).min(self.total_frames - 1);
        let (l0, r0) = self.load_frame(idx);
        if next == idx || next >= end {
            // At EOF, or the next frame is not buffered yet: hold this frame
            // rather than counting an underrun for the interpolation partner.
            return (l0, r0);
        }
        let frac = (pos - idx as f64) as f32;
        let (l1, r1) = self.load_frame(next);
        (l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac)
    }

    #[inline]
    fn bump_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
        GLOBAL_UNDERRUNS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }
}

/// A streaming clip source: owns the ring and the decoder thread. Dropping it
/// stops and joins the thread (this happens on the control / graveyard thread,
/// never the audio callback).
///
/// One source serves one playback cursor: the ring follows a single reader.
/// Clips that may sound at the same time need their own source (see
/// `RuntimeProject::build`).
pub struct StreamingSource {
    ring: Arc<StreamingRing>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for StreamingSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingSource")
            .field("sample_rate", &self.ring.sample_rate)
            .field("total_frames", &self.ring.total_frames)
            .field("underruns", &self.ring.underruns())
            .finish()
    }
}

impl StreamingSource {
    /// Probe `path`, allocate the ring, and spawn the decoder thread.
    pub fn open(path: &str) -> Result<Self, String> {
        let capacity = RING_FRAMES;
        let info = probe_audio_file(path).map_err(|e| format!("probe failed: {e}"))?;
        if info.total_frames == 0 || info.sample_rate == 0 {
            return Err(format!("streaming source has no frames: '{path}'"));
        }

        let ring = Arc::new(StreamingRing::new(
            info.total_frames,
            info.sample_rate,
            capacity,
        ));
        let stop = Arc::new(AtomicBool::new(false));

        let worker_ring = Arc::clone(&ring);
        let worker_stop = Arc::clone(&stop);
        let worker_path = path.to_string();
        let worker = std::thread::Builder::new()
            .name("daux-stream-decoder".to_string())
            .spawn(move || {
                if let Err(e) = decode_loop(&worker_path, &worker_ring, &worker_stop) {
                    if disk_stream_debug_enabled() {
                        eprintln!("[DAUx stream] decoder exited: {e}");
                    }
                }
            })
            .map_err(|e| format!("failed to spawn stream decoder: {e}"))?;

        if disk_stream_debug_enabled() {
            eprintln!(
                "[DAUx stream] streaming '{}': {} frames @ {}Hz (ring={} frames)",
                path, info.total_frames, info.sample_rate, capacity
            );
        }

        GLOBAL_ACTIVE_SOURCES.fetch_add(1, Ordering::Relaxed);

        Ok(Self {
            ring,
            stop,
            worker: Some(worker),
        })
    }

    #[inline]
    pub fn sample_rate(&self) -> u32 {
        self.ring.sample_rate
    }

    #[inline]
    pub fn frames(&self) -> usize {
        self.ring.total_frames as usize
    }

    #[inline]
    pub fn read_interp(&self, pos: f64) -> (f32, f32) {
        self.ring.read_interp(pos)
    }

    #[inline]
    pub fn read_frame_stereo(&self, frame: usize) -> (f32, f32) {
        self.ring.read_interp(frame as f64)
    }
}

impl Drop for StreamingSource {
    fn drop(&mut self) {
        GLOBAL_ACTIVE_SOURCES.fetch_sub(1, Ordering::Relaxed);
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

/// The decoder half of a stream: a symphonia reader and decoder, plus the
/// absolute frame index the next decoded packet starts at.
struct StreamDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    channels: usize,
    sample_buf: Option<SampleBuffer<f32>>,
    /// Frames in `sample_buf` from the last [`Self::decode_next`].
    decoded: usize,
    next_frame: u64,
}

/// What one [`StreamDecoder::decode_next`] call produced.
enum Packet {
    /// `decoded` frames starting at this absolute frame index.
    Frames(u64),
    /// A packet for another track, or one the decoder skipped.
    Skipped,
    Eof,
}

impl StreamDecoder {
    fn open(path: &str) -> Result<Self, String> {
        // Through the same source as the probe, so a pre-#352 FLAC export
        // streams with its STREAMINFO corrected too (#388).
        let file = File::open(Path::new(path))
            .and_then(crate::audio_file::symphonia_file_source)
            .map_err(|e| format!("open failed: {e}"))?;
        let mss = MediaSourceStream::new(file, Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| format!("format probe failed: {e}"))?;

        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
            .ok_or_else(|| "no decodable audio track".to_string())?
            .clone();
        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| format!("decoder create failed: {e}"))?;

        Ok(Self {
            format,
            decoder,
            track_id: track.id,
            channels: track
                .codec_params
                .channels
                .map(|c| c.count())
                .unwrap_or(2)
                .max(1),
            sample_buf: None,
            decoded: 0,
            next_frame: 0,
        })
    }

    /// Reposition so the next packet starts at or before `frame`.
    fn seek(&mut self, frame: u64) {
        let seeked = self.format.seek(
            SeekMode::Accurate,
            SeekTo::TimeStamp {
                ts: frame,
                track_id: self.track_id,
            },
        );
        self.decoder.reset();
        self.next_frame = match seeked {
            Ok(to) => to.actual_ts,
            Err(_) => frame, // best-effort: some formats seek approximately
        };
    }

    fn decode_next(&mut self) -> Result<Packet, String> {
        let packet = match self.format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(Packet::Eof);
            }
            Err(SymphoniaError::ResetRequired) => {
                self.decoder.reset();
                return Ok(Packet::Skipped);
            }
            Err(e) => return Err(format!("packet read error: {e}")),
        };
        if packet.track_id() != self.track_id {
            return Ok(Packet::Skipped);
        }
        match self.decoder.decode(&packet) {
            Ok(audio_ref) => {
                let buf = self.sample_buf.get_or_insert_with(|| {
                    SampleBuffer::<f32>::new(audio_ref.capacity() as u64, *audio_ref.spec())
                });
                buf.copy_interleaved_ref(audio_ref);
                self.decoded = buf.samples().len() / self.channels;
                let first = self.next_frame;
                self.next_frame += self.decoded as u64;
                GLOBAL_BLOCKS_DECODED.fetch_add(1, Ordering::Relaxed);
                GLOBAL_FRAMES_DECODED.fetch_add(self.decoded as u64, Ordering::Relaxed);
                Ok(Packet::Frames(first))
            }
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => {
                Ok(Packet::Skipped)
            }
            Err(e) => Err(format!("decode error: {e}")),
        }
    }

    /// Stereo frame `i` of the last decoded packet (mono is duplicated).
    #[inline]
    fn frame(&self, i: usize) -> (f32, f32) {
        let Some(buf) = &self.sample_buf else {
            return (0.0, 0.0);
        };
        let samples = buf.samples();
        let base = i * self.channels;
        if self.channels == 1 {
            (samples[base], samples[base])
        } else {
            (samples[base], samples[base + 1])
        }
    }
}

/// Which way the reader has been moving, with hysteresis.
struct Direction {
    reverse: bool,
    last: Option<u64>,
    drift: i64,
}

impl Direction {
    fn new() -> Self {
        Self {
            reverse: false,
            last: None,
            drift: 0,
        }
    }

    fn observe(&mut self, reader: u64) {
        if let Some(last) = self.last {
            if reader.abs_diff(last) > DIRECTION_JUMP {
                // A seek: no evidence either way.
                self.drift = 0;
            } else {
                let step = reader as i64 - last as i64;
                self.drift =
                    (self.drift + step).clamp(-2 * DIRECTION_HYSTERESIS, 2 * DIRECTION_HYSTERESIS);
                if self.drift <= -DIRECTION_HYSTERESIS {
                    self.reverse = true;
                } else if self.drift >= DIRECTION_HYSTERESIS {
                    self.reverse = false;
                }
            }
        }
        self.last = Some(reader);
    }

    fn force_reverse(&mut self) {
        self.reverse = true;
        self.drift = -DIRECTION_HYSTERESIS;
    }
}

/// Whether the worker did work this step or should sleep.
#[derive(PartialEq, Eq)]
enum Step {
    Busy,
    Idle,
}

/// The worker's view of the ring: the decoder plus a mirror of the published
/// window, which only this thread writes.
struct Filler<'a> {
    ring: &'a StreamingRing,
    dec: StreamDecoder,
    start: u64,
    end: u64,
}

impl Filler<'_> {
    fn publish(&self) {
        self.ring.publish_window(self.start, self.end);
    }

    fn reset(&mut self, at: u64) {
        self.start = at;
        self.end = at;
        self.publish();
    }

    /// Append the packet just decoded (starting at `first`) to the top of the
    /// window, dropping frames off the bottom when the ring is full.
    fn append(&mut self, first: u64) {
        let cap = self.ring.capacity as u64;
        let new_end = first + self.dec.decoded as u64;
        if new_end <= self.end {
            return; // already buffered (decoder re-read after a seek)
        }
        if first > self.end {
            // The decoder skipped ahead: the window cannot span the hole.
            self.reset(first);
        }
        if new_end - self.start > cap {
            // Drop the bottom before its slots are rewritten.
            self.start = new_end - cap;
            if self.start > self.end {
                self.end = self.start;
            }
            self.publish();
        }
        for f in self.end.max(self.start)..new_end {
            let (l, r) = self.dec.frame((f - first) as usize);
            self.ring.store_frame(f, l, r);
        }
        self.end = new_end;
        self.publish();
    }

    /// Forward playback: keep up to `capacity - margin` frames decoded ahead.
    fn fill_forward(&mut self, reader: u64, dir: &mut Direction) -> Result<Step, String> {
        let cap = self.ring.capacity as u64;
        let total = self.ring.total_frames;
        let has_window = self.start < self.end;
        if reader < self.start || reader >= self.end + cap {
            if has_window && reader < self.start && self.start - reader <= THROTTLE_MARGIN {
                // The reader walked off the bottom edge: it is playing
                // backward (or stepped back a little). Fill below instead of
                // throwing the window away.
                dir.force_reverse();
                return Ok(Step::Busy);
            }
            // Seek reposition: the playhead moved outside what we can serve —
            // before the window, or so far ahead that catching up by decoding
            // would stall playback.
            self.dec.seek(reader);
            self.reset(self.dec.next_frame);
            if disk_stream_debug_enabled() {
                eprintln!(
                    "[DAUx stream] reposition -> frame {} (reader={reader})",
                    self.start
                );
            }
            return Ok(Step::Busy);
        }

        let target_end = (reader + cap - THROTTLE_MARGIN).min(total);
        if self.end >= target_end {
            return Ok(Step::Idle);
        }
        if self.dec.next_frame != self.end {
            // The decoder was used for a backward fill; continue from the top.
            self.dec.seek(self.end);
            if self.dec.next_frame > self.end {
                self.reset(self.dec.next_frame);
            }
        }
        match self.dec.decode_next()? {
            Packet::Frames(first) => self.append(first),
            Packet::Skipped => {}
            // Nothing more to decode; idle until a seek wakes us.
            Packet::Eof => return Ok(Step::Idle),
        }
        Ok(Step::Busy)
    }

    /// Backward playback: keep the window reaching as far below the reader as
    /// the ring allows, filling it a chunk at a time.
    fn fill_backward(&mut self, reader: u64, stop: &AtomicBool) -> Result<Step, String> {
        let cap = self.ring.capacity as u64;
        let total = self.ring.total_frames;
        let chunk = REVERSE_CHUNK.min(cap / 4).max(1);
        if reader >= self.end || reader + chunk < self.start {
            // Reposition just above the reader; the fill below brings the
            // frames it is about to read.
            let top = (reader + THROTTLE_MARGIN).min(total);
            self.reset(top);
            if disk_stream_debug_enabled() {
                eprintln!("[DAUx stream] reverse reposition -> frame {top} (reader={reader})");
            }
            return Ok(Step::Busy);
        }
        if self.start == 0 {
            return Ok(Step::Idle);
        }
        // Frames already buffered below the reader. A new chunk must leave
        // the reader at least `THROTTLE_MARGIN` under the (lowered) top.
        let depth = reader.saturating_sub(self.start);
        if depth + chunk.min(self.start) + THROTTLE_MARGIN >= cap {
            return Ok(Step::Idle);
        }

        let old_start = self.start;
        let new_start = old_start.saturating_sub(chunk);
        let new_end = self.end.min(new_start + cap);
        if new_end < self.end {
            // Drop the top before its slots are rewritten.
            self.end = new_end;
            self.publish();
        }
        self.dec.seek(new_start);
        let landed = self.dec.next_frame;
        let mut reached = landed;
        while reached < old_start {
            if stop.load(Ordering::Acquire) {
                return Ok(Step::Busy);
            }
            match self.dec.decode_next()? {
                Packet::Frames(first) => {
                    if first != reached {
                        break; // the decoder skipped: no contiguous fill
                    }
                    let to = (first + self.dec.decoded as u64).min(old_start);
                    for f in first.max(new_start)..to {
                        let (l, r) = self.dec.frame((f - first) as usize);
                        self.ring.store_frame(f, l, r);
                    }
                    reached = first + self.dec.decoded as u64;
                }
                Packet::Skipped => {}
                Packet::Eof => break,
            }
        }
        let filled_from = landed.max(new_start);
        if reached < old_start || filled_from >= old_start {
            // No contiguous progress (approximate seek or a decode hole):
            // leave the window as it is rather than spin on the same chunk.
            return Ok(Step::Idle);
        }
        self.start = filled_from;
        self.publish();
        Ok(Step::Busy)
    }
}

/// Decoder thread body: open the file, then loop decoding and filling the ring
/// in the reader's direction, repositioning on seek and throttling so the
/// buffer stays ahead of the playhead without overrunning unread frames.
fn decode_loop(path: &str, ring: &StreamingRing, stop: &AtomicBool) -> Result<(), String> {
    let mut filler = Filler {
        ring,
        dec: StreamDecoder::open(path)?,
        start: 0,
        end: 0,
    };
    let mut dir = Direction::new();

    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let reader = ring.reader_frame();
        dir.observe(reader);
        let step = if dir.reverse {
            filler.fill_backward(reader, stop)?
        } else {
            filler.fill_forward(reader, &mut dir)?
        };
        if step == Step::Idle {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the ring's producer side by hand (no decoder thread) to validate
    /// the window + underrun logic deterministically.
    fn ring(capacity: usize, total: u64) -> StreamingRing {
        StreamingRing::new(total, 48_000, capacity)
    }

    /// Write `[from, to)` with each frame's own index and publish the window
    /// the way the forward fill does.
    fn fill(r: &StreamingRing, from: u64, to: u64) {
        for f in from..to {
            r.store_frame(f, f as f32, -(f as f32));
        }
        r.publish_window(to.saturating_sub(r.capacity as u64).max(from), to);
    }

    #[test]
    fn read_inside_window_returns_buffered_sample() {
        let r = ring(16, 1000);
        fill(&r, 0, 10);
        let (l, _r) = r.read_interp(5.0);
        assert_eq!(l, 5.0);
        assert_eq!(r.underruns(), 0);
    }

    #[test]
    fn read_ahead_of_written_is_underrun() {
        let r = ring(16, 1000);
        fill(&r, 0, 4);
        let (l, rr) = r.read_interp(8.0);
        assert_eq!((l, rr), (0.0, 0.0));
        assert_eq!(r.underruns(), 1);
    }

    #[test]
    fn read_behind_window_start_is_underrun() {
        // capacity 16: after writing up to frame 40 the window is [24, 40).
        let r = ring(16, 1000);
        fill(&r, 0, 40);
        assert_eq!(r.window(), (24, 40));
        let (l, rr) = r.read_interp(5.0);
        assert_eq!((l, rr), (0.0, 0.0));
        assert_eq!(r.underruns(), 1);
        // A frame inside the window still reads fine.
        let (l2, _) = r.read_interp(30.0);
        assert_eq!(l2, 30.0);
    }

    /// #161: after a reposition the slots below the new start still hold audio
    /// from the old position. The old model published only the write head and
    /// derived the window as `[head - capacity, head)`, so a backward read just
    /// below a reposition returned that stale audio as valid, with no underrun.
    #[test]
    fn reposition_does_not_expose_stale_frames_below_the_new_start() {
        let r = ring(16, 1000);
        fill(&r, 0, 40); // window [24, 40); slot of frame 99 holds frame 35
        r.publish_window(100, 100); // reposition to 100
        for f in 100..104 {
            r.store_frame(f, f as f32, -(f as f32));
        }
        r.publish_window(100, 104);

        let (l, rr) = r.read_interp(99.0);
        assert_eq!((l, rr), (0.0, 0.0), "stale frame served after reposition");
        assert_eq!(r.underruns(), 1);
        let (l, _) = r.read_interp(101.0);
        assert_eq!(l, 101.0);
    }

    #[test]
    fn interpolates_between_frames() {
        let r = ring(16, 1000);
        fill(&r, 0, 10);
        // Halfway between frame 2 (=2.0) and 3 (=3.0).
        let (l, _) = r.read_interp(2.5);
        assert!((l - 2.5).abs() < 1e-6);
    }

    #[test]
    fn past_end_is_silent_not_underrun() {
        let r = ring(16, 4);
        fill(&r, 0, 4);
        let (l, rr) = r.read_interp(10.0);
        assert_eq!((l, rr), (0.0, 0.0));
        assert_eq!(r.underruns(), 0);
    }

    #[test]
    fn last_frame_holds_without_underrun() {
        // Reading the final frame must not count its (nonexistent) interp
        // partner as an underrun.
        let r = ring(16, 4);
        fill(&r, 0, 4);
        let (l, _) = r.read_interp(3.0);
        assert_eq!(l, 3.0);
        assert_eq!(r.underruns(), 0);
    }

    #[test]
    fn window_word_round_trips() {
        for (s, e) in [(0, 0), (5, 9), (1 << 40, (1 << 40) + 262_144)] {
            assert_eq!(unpack_window(pack_window(s, e)), (s, e));
        }
    }

    #[test]
    fn direction_follows_net_motion_and_ignores_seeks() {
        let mut d = Direction::new();
        for p in (0..10_000u64).step_by(100) {
            d.observe(p);
        }
        assert!(!d.reverse);
        for p in (0..5_000u64).rev().step_by(100) {
            d.observe(500_000 + p);
        }
        assert!(d.reverse, "backward motion after a seek reads as reverse");
        // Grain pairs that step back and forth but drift forward.
        let mut p = 600_000u64;
        for _ in 0..200 {
            d.observe(p + 512);
            d.observe(p);
            p += 64;
        }
        assert!(!d.reverse);
    }

    // ── Decoder-thread tests on a real file ─────────────────────────────────

    /// 16-bit stereo WAV whose samples encode their own frame index, so any
    /// value read back says exactly which frame it came from. Never zero, so a
    /// hit cannot be mistaken for the silence of an underrun.
    fn write_index_wav(name: &str, frames: u64) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fb_stream_{}_{}_{name}.wav",
            std::process::id(),
            frames
        ));
        let data_len = (frames * 4) as u32;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&2u16.to_le_bytes()); // stereo
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for f in 0..frames {
            let l = ((f & 0x3fff) + 1) as i16;
            let r = (((f >> 14) & 0x3fff) + 1) as i16;
            bytes.extend_from_slice(&l.to_le_bytes());
            bytes.extend_from_slice(&r.to_le_bytes());
        }
        std::fs::write(&path, bytes).expect("write test wav");
        path
    }

    fn decode_index((l, r): (f32, f32)) -> u64 {
        let lo = (l * 32768.0).round() as i64 - 1;
        let hi = (r * 32768.0).round() as i64 - 1;
        ((hi as u64) << 14) | lo as u64
    }

    /// Read `frame`, waiting for the worker if it is not buffered yet. Returns
    /// whether the first attempt already hit.
    fn read_waiting(source: &StreamingSource, frame: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut first = true;
        loop {
            let before = source.ring.underruns();
            let value = source.read_interp(frame as f64);
            if source.ring.underruns() == before {
                assert_eq!(
                    decode_index(value),
                    frame,
                    "frame {frame} read back audio from another frame"
                );
                return first;
            }
            first = false;
            assert!(
                std::time::Instant::now() < deadline,
                "frame {frame} never became readable"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// #161: a reversed clip on a streamed file. Every read must be the right
    /// frame (or an underrun), and the backward prefetch has to keep up: the
    /// forward-only worker repositioned on every step and played ~8192-frame
    /// bursts between long stretches of stale or silent audio.
    #[test]
    fn reverse_playback_reads_the_right_frames_and_prefetches_backward() {
        let frames = 600_000u64;
        let path = write_index_wav("reverse", frames);
        let source = StreamingSource::open(path.to_str().unwrap()).expect("open stream");

        let mut steps = 0u32;
        let mut waits = 0u32;
        let mut frame = frames - 1;
        loop {
            steps += 1;
            if !read_waiting(&source, frame) {
                waits += 1;
            }
            if frame < 509 {
                break;
            }
            frame -= 509;
        }
        drop(source);
        let _ = std::fs::remove_file(&path);
        assert!(
            waits * 10 < steps,
            "backward prefetch fell behind: waited on {waits} of {steps} reads"
        );
    }

    /// #161: on a forward clip, a short step back to just before a recent
    /// reposition point must not play the stale audio left in those slots.
    #[test]
    fn short_step_back_after_reposition_reads_the_right_frames() {
        let frames = 900_000u64;
        let path = write_index_wav("stepback", frames);
        let source = StreamingSource::open(path.to_str().unwrap()).expect("open stream");

        for f in (0..20_000u64).step_by(97) {
            read_waiting(&source, f);
        }
        // Jump far ahead (a reposition), play a little, then step back past
        // the reposition point.
        for f in (700_000..702_000u64).step_by(97) {
            read_waiting(&source, f);
        }
        for f in (695_000..701_000u64).step_by(97) {
            read_waiting(&source, f);
        }
        drop(source);
        let _ = std::fs::remove_file(&path);
    }
}

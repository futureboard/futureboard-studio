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
//!
//! ## Cues (#380)
//!
//! The window follows the reader, so a jump — a loop wrap, a seek back —
//! always lands outside it, and the first block after the jump used to play
//! silence while the worker repositioned. A *cue* is a small second buffer
//! (`CUE_FRAMES`) the worker fills ahead of time around a frame the control
//! side or the render path says the reader is about to jump to: the loop
//! start of a looped clip ([`StreamCue::Loop`], requested from the render path
//! every block with a relaxed compare-and-store) and a transport seek target
//! ([`StreamCue::Seek`], requested from the control thread before the seek
//! command is sent). Reads that miss the window fall back to the cues, and
//! the reader keeps publishing `reader_frame`, so the worker repositions the
//! window while the cue covers the jump. A cue is only rewritten when its
//! request changes, after its window was unpublished (the loop cue only once
//! the window has caught up with the reader, so the reader is not on it; a
//! read racing a new seek request can at worst get one stale sample, never
//! UB — the slots are atomics).
//!
//! ## Offline reads (#370)
//!
//! An export runs far faster than real time and must be sample-identical to
//! the in-memory path, so silence on a miss is never acceptable there. A source
//! switched to blocking reads ([`StreamingSource::set_blocking`]) waits for
//! the worker instead — wakes it, then yields/sleeps until the frame and its
//! interpolation partner are buffered — and records a failure (reported by the
//! exporter as an error) if the decoder stopped or did not deliver in
//! `BLOCKING_READ_TIMEOUT`. Only the offline renderer's private streams are
//! switched; the realtime read path never waits.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// Frames per cue. 32768 frames ≈ 0.68 s @ 48 kHz (256 KB): the time the
/// worker has to reposition the main window after the reader jumps into it.
const CUE_FRAMES: usize = 1 << 15;

/// Frames a cue keeps on the near side of its target, for readers that start
/// a little before the frame they were cued at (a stretcher priming its
/// input, a grain pair).
const CUE_LEAD_IN: u64 = 4096;

/// `Cue::request` when nothing is requested.
const NO_CUE: u64 = u64::MAX;

const CUE_COUNT: usize = 2;

/// How long an offline (blocking) read waits for the decoder before the
/// export fails with an error instead of writing silence.
const BLOCKING_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Which prefetch slot a cue request fills (see "Cues" in the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamCue {
    /// A transport seek target, requested from the control thread.
    Seek = 0,
    /// The loop start of a looped clip, requested from the render path.
    Loop = 1,
}

/// Pack a cue target for `Cue::request`: the frame, and whether the reader
/// moves backward from it (a reversed clip).
#[inline]
fn cue_request_word(frame: f64, reverse: bool, total_frames: u64) -> u64 {
    let frame = if frame.is_finite() && frame > 0.0 {
        (frame as u64).min(total_frames.saturating_sub(1))
    } else {
        0
    };
    (frame << 1) | reverse as u64
}

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

fn atomic_frames(frames: usize) -> Box<[AtomicU32]> {
    (0..frames * 2)
        .map(|_| AtomicU32::new(0))
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

/// One prefetched region outside the main window (see "Cues" in the module
/// docs). Slot `i` holds frame `start + i` of the published window.
#[derive(Debug)]
struct Cue {
    /// Interleaved stereo f32 bits; length `CUE_FRAMES * 2`.
    data: Box<[AtomicU32]>,
    /// Packed `[start, end)` of the frames the cue holds.
    window: AtomicU64,
    /// What the cue should hold: [`cue_request_word`] or [`NO_CUE`].
    request: AtomicU64,
}

impl Cue {
    fn new() -> Self {
        Self {
            data: atomic_frames(CUE_FRAMES),
            window: AtomicU64::new(pack_window(0, 0)),
            request: AtomicU64::new(NO_CUE),
        }
    }

    #[inline]
    fn window(&self) -> (u64, u64) {
        unpack_window(self.window.load(Ordering::Acquire))
    }

    #[inline]
    fn load_frame(&self, start: u64, idx: u64) -> (f32, f32) {
        let slot = (idx - start) as usize * 2;
        (
            f32::from_bits(self.data[slot].load(Ordering::Relaxed)),
            f32::from_bits(self.data[slot + 1].load(Ordering::Relaxed)),
        )
    }

    #[inline]
    fn store_frame(&self, start: u64, idx: u64, l: f32, r: f32) {
        let slot = (idx - start) as usize * 2;
        self.data[slot].store(l.to_bits(), Ordering::Relaxed);
        self.data[slot + 1].store(r.to_bits(), Ordering::Relaxed);
    }
}

/// What a read found.
enum Lookup {
    /// The frame and its interpolation partner were buffered.
    Hit((f32, f32)),
    /// The frame was buffered but its interpolation partner was not: the
    /// frame itself, held.
    Partial((f32, f32)),
    Miss,
}

#[inline]
fn lerp_frames(pos: f64, idx: u64, (l0, r0): (f32, f32), (l1, r1): (f32, f32)) -> (f32, f32) {
    // The same arithmetic as `audio_source::sample_source_stereo`, so a
    // streamed read is bit-identical to an in-memory one.
    let frac = (pos - idx as f64) as f32;
    (l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac)
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
    /// Prefetched jump targets, indexed by [`StreamCue`].
    cues: [Cue; CUE_COUNT],
    /// Frame at which the decoder reached the end of the stream, if it ended
    /// before `total_frames` (an estimated length); `u64::MAX` until then.
    eof_frame: AtomicU64,
    /// Offline reads: wait for the decoder instead of returning silence.
    blocking: AtomicBool,
    /// First offline read that could not be served: `frame + 1`, or 0.
    blocking_failed_frame: AtomicU64,
    /// The decoder thread has exited (error or stop).
    worker_exited: AtomicBool,
}

impl StreamingRing {
    fn new(total_frames: u64, sample_rate: u32, capacity: usize) -> Self {
        let capacity = capacity.clamp(2, WINDOW_LEN_MASK as usize);
        Self {
            capacity,
            total_frames,
            sample_rate,
            data: atomic_frames(capacity),
            window: AtomicU64::new(pack_window(0, 0)),
            reader_frame: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            cues: [Cue::new(), Cue::new()],
            eof_frame: AtomicU64::new(u64::MAX),
            blocking: AtomicBool::new(false),
            blocking_failed_frame: AtomicU64::new(0),
            worker_exited: AtomicBool::new(false),
        }
    }

    /// Ask the worker to prefetch around `frame` into `cue`. Realtime-safe:
    /// one relaxed load, and a release store only when the target changed.
    #[inline]
    pub fn request_cue(&self, cue: StreamCue, frame: f64, reverse: bool) {
        let word = cue_request_word(frame, reverse, self.total_frames);
        let slot = &self.cues[cue as usize].request;
        if slot.load(Ordering::Relaxed) != word {
            slot.store(word, Ordering::Release);
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

    /// Frame `idx` from a cue, if one holds it.
    #[inline]
    fn cue_frame(&self, idx: u64) -> Option<(f32, f32)> {
        self.cues.iter().find_map(|cue| {
            let (start, end) = cue.window();
            (idx >= start && idx < end).then(|| cue.load_frame(start, idx))
        })
    }

    /// Frame `idx` from the window or a cue, if either holds it.
    #[inline]
    fn buffered_frame(&self, idx: u64) -> Option<(f32, f32)> {
        let (start, end) = self.window();
        if idx >= start && idx < end {
            return Some(self.load_frame(idx));
        }
        self.cue_frame(idx)
    }

    /// The interpolated sample at `pos` (`idx = floor(pos)`, in range) from
    /// the window, falling back to the cues. Atomic loads only.
    #[inline]
    fn lookup(&self, pos: f64, idx: u64) -> Lookup {
        // Next frame for interpolation, clamped to the last real frame.
        let next = (idx + 1).min(self.total_frames - 1);
        let (start, end) = self.window();
        let first = if idx >= start && idx < end {
            let first = self.load_frame(idx);
            if next == idx {
                return Lookup::Hit(first);
            }
            if next < end {
                return Lookup::Hit(lerp_frames(pos, idx, first, self.load_frame(next)));
            }
            first
        } else {
            match self.cue_frame(idx) {
                Some(first) => first,
                None => return Lookup::Miss,
            }
        };
        if next == idx {
            return Lookup::Hit(first);
        }
        match self.buffered_frame(next) {
            Some(second) => Lookup::Hit(lerp_frames(pos, idx, first, second)),
            None => Lookup::Partial(first),
        }
    }

    /// Read a linearly-interpolated stereo sample at fractional frame `pos`.
    /// Realtime-safe: relaxed atomic loads only. Reads the window, else a cue;
    /// a frame in neither returns silence and bumps the underrun counters.
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

        match self.lookup(pos, idx) {
            // A missing interpolation partner (at the window's edge) holds
            // this frame rather than counting an underrun.
            Lookup::Hit(value) | Lookup::Partial(value) => {
                GLOBAL_HITS.fetch_add(1, Ordering::Relaxed);
                value
            }
            Lookup::Miss => {
                self.bump_underrun();
                (0.0, 0.0)
            }
        }
    }

    /// Offline read: like [`Self::read_interp`], but waits for the decoder
    /// (waking it through `wake`) until the frame and its interpolation
    /// partner are buffered, so an export never writes the silence of a
    /// frame that simply was not decoded yet. A read that cannot be served —
    /// the decoder stopped, or did not deliver in `BLOCKING_READ_TIMEOUT` —
    /// records a failure for the exporter and returns silence; once failed,
    /// reads stop waiting. Never called on the audio thread.
    fn read_interp_blocking(&self, pos: f64, wake: impl Fn()) -> (f32, f32) {
        GLOBAL_READS.fetch_add(1, Ordering::Relaxed);
        if pos < 0.0 || self.total_frames == 0 {
            return (0.0, 0.0);
        }
        let idx = pos.floor() as u64;
        if idx >= self.total_frames {
            return (0.0, 0.0);
        }
        let next = (idx + 1).min(self.total_frames - 1);
        let mut want = idx;
        let mut deadline: Option<Instant> = None;
        let mut attempts = 0u32;
        loop {
            self.reader_frame.store(want, Ordering::Release);
            let eof = self.eof_frame.load(Ordering::Acquire);
            match self.lookup(pos, idx) {
                Lookup::Hit(value) => {
                    GLOBAL_HITS.fetch_add(1, Ordering::Relaxed);
                    return value;
                }
                // The partner is past the end the decoder found: hold, as
                // the in-memory path does on its last frame.
                Lookup::Partial(value) if next >= eof => {
                    GLOBAL_HITS.fetch_add(1, Ordering::Relaxed);
                    return value;
                }
                // Ask for the partner; the frame itself is already there.
                Lookup::Partial(_) => want = next,
                // The stream ended before this frame (an estimated length).
                Lookup::Miss if idx >= eof => return (0.0, 0.0),
                Lookup::Miss => want = idx,
            }
            if self.blocking_failed_frame.load(Ordering::Acquire) != 0 {
                return (0.0, 0.0);
            }
            let now = Instant::now();
            let deadline = *deadline.get_or_insert(now + BLOCKING_READ_TIMEOUT);
            if self.worker_exited.load(Ordering::Acquire) || now >= deadline {
                self.bump_underrun();
                let _ = self.blocking_failed_frame.compare_exchange(
                    0,
                    idx + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                return (0.0, 0.0);
            }
            wake();
            attempts = attempts.saturating_add(1);
            if attempts < 64 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(100));
            }
        }
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
    /// Where the clips reading this stream sit on the timeline, so a seek
    /// can cue its target (#380). Control thread only: written by
    /// `RuntimeProject::build`, read by [`Self::prefetch_timeline`].
    clip_spans: parking_lot::Mutex<Vec<StreamClipSpan>>,
}

/// One clip's mapping from timeline samples to frames of its stream: the
/// resample path's mapping (`resolved_clip_source_frame` in the render
/// kernel), which the stretch paths read close around. Used to cue a stream
/// before the reader jumps; an approximate frame only costs a miss.
#[derive(Debug, Clone)]
pub(crate) struct StreamClipSpan {
    start_sample: u64,
    duration_samples: u64,
    output_sample_rate: u32,
    offset_seconds: f64,
    read_rate: f64,
    reverse: bool,
    warp_segments: Vec<crate::runtime::RuntimeWarpSegment>,
}

impl StreamClipSpan {
    /// Control thread only (clones the warp map).
    pub(crate) fn from_clip(clip: &crate::runtime::RuntimeClip, output_sample_rate: u32) -> Self {
        Self {
            start_sample: clip.start_sample,
            duration_samples: clip.duration_samples,
            output_sample_rate,
            offset_seconds: clip.offset_seconds,
            read_rate: cue_read_rate(clip),
            reverse: clip.reverse,
            warp_segments: clip.warp_segments.clone(),
        }
    }

    fn source_frame(&self, rel: u64, source_sample_rate: u32) -> f64 {
        cue_source_frame(
            rel,
            self.duration_samples,
            self.output_sample_rate,
            self.offset_seconds,
            self.read_rate,
            self.reverse,
            &self.warp_segments,
            source_sample_rate,
        )
    }
}

/// Source frames per output frame a clip reads at: its stretch ratio on the
/// stretch paths, its resample rate otherwise (mirrors the render kernel).
#[inline]
fn cue_read_rate(clip: &crate::runtime::RuntimeClip) -> f64 {
    if matches!(
        clip.processor,
        crate::runtime::ClipDspProcessor::PhaseVocoderBasic
    ) && clip.warp_segments.is_empty()
    {
        1.0 / clip.effective_time_ratio.max(0.01) as f64
    } else {
        clip.source_read_rate as f64
    }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn cue_source_frame(
    rel: u64,
    duration_samples: u64,
    output_sample_rate: u32,
    offset_seconds: f64,
    read_rate: f64,
    reverse: bool,
    warp_segments: &[crate::runtime::RuntimeWarpSegment],
    source_sample_rate: u32,
) -> f64 {
    if let Some(frame) =
        crate::runtime::map_warp_source_frame(rel, duration_samples, reverse, warp_segments)
    {
        return frame;
    }
    let advance = if reverse {
        duration_samples.saturating_sub(1).saturating_sub(rel)
    } else {
        rel
    } as f64;
    (offset_seconds + advance / output_sample_rate.max(1) as f64 * read_rate)
        * source_sample_rate as f64
}

/// Cue a looped, disk-streamed clip's loop start (#380): `rel` is the loop
/// start relative to the clip. Realtime-safe — arithmetic and the atomics of
/// [`StreamingRing::request_cue`] only; no allocation, lock, or wakeup (the
/// worker polls).
#[inline]
pub(crate) fn cue_loop_start(
    stream: &StreamingSource,
    clip: &crate::runtime::RuntimeClip,
    rel: u64,
    output_sample_rate: u32,
) {
    let frame = cue_source_frame(
        rel,
        clip.duration_samples,
        output_sample_rate,
        clip.offset_seconds,
        cue_read_rate(clip),
        clip.reverse,
        &clip.warp_segments,
        stream.sample_rate(),
    );
    stream
        .ring
        .request_cue(StreamCue::Loop, frame, clip.reverse);
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
                let result = decode_loop(&worker_path, &worker_ring, &worker_stop);
                // Offline reads stop waiting for a decoder that is gone.
                worker_ring.worker_exited.store(true, Ordering::Release);
                if let Err(e) = result {
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
            clip_spans: parking_lot::Mutex::new(Vec::new()),
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

    /// Interpolated stereo sample at `pos`. Realtime streams never wait (a
    /// miss is silence); an offline stream waits for its decoder (#370).
    #[inline]
    pub fn read_interp(&self, pos: f64) -> (f32, f32) {
        if self.ring.blocking.load(Ordering::Relaxed) {
            return self.ring.read_interp_blocking(pos, || self.wake_worker());
        }
        self.ring.read_interp(pos)
    }

    #[inline]
    pub fn read_frame_stereo(&self, frame: usize) -> (f32, f32) {
        self.read_interp(frame as f64)
    }

    /// Switch this stream to offline reads that wait for the decoder (#370).
    /// Only for a stream no realtime graph reads: the offline renderer opens
    /// its own.
    pub fn set_blocking(&self, blocking: bool) {
        self.ring.blocking.store(blocking, Ordering::Relaxed);
    }

    /// Why an offline read could not be served, once one could not.
    pub fn blocking_failure(&self) -> Option<String> {
        let failed = self.ring.blocking_failed_frame.load(Ordering::Acquire);
        if failed == 0 {
            return None;
        }
        let frame = failed - 1;
        Some(if self.ring.worker_exited.load(Ordering::Acquire) {
            format!(
                "the disk-stream decoder stopped before frame {frame} of {} was decoded",
                self.ring.total_frames
            )
        } else {
            format!(
                "the disk-stream decoder did not deliver frame {frame} within {} s",
                BLOCKING_READ_TIMEOUT.as_secs()
            )
        })
    }

    /// Prefetch around `frame` into `cue` now, waking the decoder. Control
    /// thread (the wakeup is a syscall); the render path uses
    /// [`StreamingRing::request_cue`] through [`cue_loop_start`].
    pub fn prefetch(&self, cue: StreamCue, frame: f64, reverse: bool) {
        self.ring.request_cue(cue, frame, reverse);
        self.wake_worker();
    }

    /// Replace the clip mappings [`Self::prefetch_timeline`] uses. Control
    /// thread only.
    pub(crate) fn set_clip_spans(&self, spans: Vec<StreamClipSpan>) {
        *self.clip_spans.lock() = spans;
    }

    /// The transport is about to jump to `position_seconds`: cue the frame
    /// the clip there will read — or, between clips, the first frame of the
    /// next clip of this stream — before the audio thread gets there (#380).
    /// Control thread only.
    pub fn prefetch_timeline(&self, position_seconds: f64) {
        let target = {
            let spans = self.clip_spans.lock();
            let mut covering = None;
            let mut upcoming: Option<(u64, &StreamClipSpan)> = None;
            for span in spans.iter() {
                // The same truncation the Seek command applies.
                let t = (position_seconds.max(0.0) * span.output_sample_rate as f64) as u64;
                let end = span.start_sample.saturating_add(span.duration_samples);
                if t >= span.start_sample && t < end {
                    covering = Some((
                        span.source_frame(t - span.start_sample, self.ring.sample_rate),
                        span.reverse,
                    ));
                    break;
                }
                if span.start_sample > t
                    && upcoming.is_none_or(|(start, _)| span.start_sample < start)
                {
                    upcoming = Some((span.start_sample, span));
                }
            }
            covering.or_else(|| {
                upcoming
                    .map(|(_, span)| (span.source_frame(0, self.ring.sample_rate), span.reverse))
            })
        };
        if let Some((frame, reverse)) = target {
            self.prefetch(StreamCue::Seek, frame, reverse);
        }
    }

    fn wake_worker(&self) {
        if let Some(worker) = &self.worker {
            worker.thread().unpark();
        }
    }
}

impl Drop for StreamingSource {
    fn drop(&mut self) {
        GLOBAL_ACTIVE_SOURCES.fetch_sub(1, Ordering::Relaxed);
        self.stop.store(true, Ordering::Release);
        self.wake_worker();
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
    /// The cue request each cue was last filled for.
    cue_served: [u64; CUE_COUNT],
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
            // Nothing more to decode; idle until a seek wakes us. Record
            // where the stream really ended, so offline reads past it (an
            // estimated length) do not wait for frames that do not exist.
            Packet::Eof => {
                self.ring
                    .eof_frame
                    .fetch_min(self.dec.next_frame, Ordering::AcqRel);
                return Ok(Step::Idle);
            }
        }
        Ok(Step::Busy)
    }

    /// Fill `cue` if its request changed since it was last served. Returns
    /// whether it did any work. The cue's window is unpublished before its
    /// slots are rewritten and republished once they hold the new frames; the
    /// decoder's position is left wherever the fill ended (the window fills
    /// reseek when they find it moved).
    fn serve_cue(&mut self, cue: StreamCue, stop: &AtomicBool) -> Result<bool, String> {
        let slot = &self.ring.cues[cue as usize];
        let request = slot.request.load(Ordering::Acquire);
        if request == self.cue_served[cue as usize] {
            return Ok(false);
        }
        self.cue_served[cue as usize] = request;
        slot.window.store(pack_window(0, 0), Ordering::Release);
        if request == NO_CUE {
            return Ok(true);
        }

        let total = self.ring.total_frames;
        let target = request >> 1;
        let reverse = request & 1 == 1;
        let span = CUE_FRAMES as u64;
        // Most of the cue lies the way the reader will move from the target.
        let from = if reverse {
            (target + CUE_LEAD_IN + 1).saturating_sub(span)
        } else {
            target.saturating_sub(CUE_LEAD_IN)
        };
        let to = from.saturating_add(span).min(total);
        if from >= to {
            return Ok(true);
        }

        self.dec.seek(from);
        let landed = self.dec.next_frame;
        let cue_start = landed.max(from);
        let mut reached = landed;
        while reached < to {
            if stop.load(Ordering::Acquire) {
                return Ok(true);
            }
            match self.dec.decode_next()? {
                Packet::Frames(first) => {
                    if first != reached {
                        break; // the decoder skipped: keep what is contiguous
                    }
                    let upto = (first + self.dec.decoded as u64).min(to);
                    for f in first.max(cue_start)..upto {
                        let (l, r) = self.dec.frame((f - first) as usize);
                        slot.store_frame(cue_start, f, l, r);
                    }
                    reached = first + self.dec.decoded as u64;
                }
                Packet::Skipped => {}
                Packet::Eof => break,
            }
        }
        let cue_end = reached.min(to);
        if cue_end > cue_start {
            slot.window
                .store(pack_window(cue_start, cue_end), Ordering::Release);
        }
        if disk_stream_debug_enabled() {
            eprintln!(
                "[DAUx stream] {cue:?} cue -> frames {cue_start}..{cue_end} (target={target})"
            );
        }
        Ok(true)
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
///
/// A seek cue is served as soon as it is requested (the reader is about to
/// jump there); the loop cue once the window has caught up, so it never
/// delays the frames the reader needs now. Idle waits park, so a seek
/// prefetch or an offline read can wake the worker at once.
fn decode_loop(path: &str, ring: &StreamingRing, stop: &AtomicBool) -> Result<(), String> {
    let mut filler = Filler {
        ring,
        dec: StreamDecoder::open(path)?,
        start: 0,
        end: 0,
        cue_served: [NO_CUE; CUE_COUNT],
    };
    let mut dir = Direction::new();

    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if filler.serve_cue(StreamCue::Seek, stop)? {
            continue;
        }
        let reader = ring.reader_frame();
        dir.observe(reader);
        let step = if dir.reverse {
            filler.fill_backward(reader, stop)?
        } else {
            filler.fill_forward(reader, &mut dir)?
        };
        if step == Step::Idle {
            if filler.serve_cue(StreamCue::Loop, stop)? {
                continue;
            }
            std::thread::park_timeout(Duration::from_millis(2));
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

    // ── Cues (#380) and offline reads (#370) ────────────────────────────────

    /// Wait until `ready` holds, failing after 10 s.
    fn wait_for(what: &str, ready: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn window_covers(source: &StreamingSource, from: u64, to: u64) -> bool {
        let (start, end) = source.ring.window();
        start <= from && to <= end
    }

    fn cue_covers(source: &StreamingSource, cue: StreamCue, from: u64, to: u64) -> bool {
        let (start, end) = source.ring.cues[cue as usize].window();
        start <= from && to <= end
    }

    /// The worker has filled the window as far ahead of the reader as it
    /// ever does, so the window has left everything well behind the reader.
    fn settled(source: &StreamingSource) -> bool {
        let ring = &source.ring;
        let reader = ring.reader_frame();
        let target = (reader + ring.capacity as u64 - THROTTLE_MARGIN).min(ring.total_frames);
        let (start, end) = ring.window();
        start <= reader && end >= target
    }

    /// Play `[from, from + frames)` the way the callback does — 256-frame
    /// blocks, without waiting for the worker — at about 2.5x real time.
    /// Every frame must be the right one: never the silence of an underrun.
    fn play(source: &StreamingSource, from: u64, frames: u64) {
        let before = source.ring.underruns();
        for block in (from..from + frames).step_by(256) {
            for f in block..(block + 256).min(from + frames) {
                let value = source.read_interp(f as f64);
                assert_eq!(
                    decode_index(value),
                    f,
                    "frame {f} read back other audio or the silence of an underrun"
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(source.ring.underruns(), before, "underruns while playing");
    }

    /// #380: a loop wrap or a seek back to a cued frame plays on without a
    /// silent block. Before cues the first read at the new position missed
    /// the window (still at the old position) and the block was silence; now
    /// the first reads come from the cue while the worker repositions the
    /// window, which takes over before the cue runs out.
    #[test]
    fn a_cued_jump_plays_on_without_an_underrun() {
        let frames = 900_000u64;
        let path = write_index_wav("cue", frames);
        let source = StreamingSource::open(path.to_str().unwrap()).expect("open stream");

        for (cue, away, target) in [
            (StreamCue::Loop, 500_000u64, 100_000u64),
            (StreamCue::Seek, 300_000, 40_000),
        ] {
            // Play somewhere else until the window has left the target.
            read_waiting(&source, away);
            wait_for("the window to settle", || settled(&source));
            play(&source, away, 4096);
            wait_for("the window to settle", || settled(&source));
            assert!(
                !window_covers(&source, target, target + 1),
                "the jump target must not still be in the window"
            );

            match cue {
                // The render path's request: no wakeup, the worker polls.
                StreamCue::Loop => source.ring.request_cue(cue, target as f64, false),
                // The control thread's seek prefetch.
                StreamCue::Seek => source.prefetch(cue, target as f64, false),
            }
            wait_for("the cue to fill", || {
                cue_covers(&source, cue, target, target + 256)
            });

            // The jump, and on past the end of the cue.
            play(&source, target, CUE_FRAMES as u64 * 2);
        }
        drop(source);
        let _ = std::fs::remove_file(&path);
    }

    /// #380 end to end: the render path cues a looped streamed clip's loop
    /// start, and every wrap plays it from the cue — while the stream's
    /// window is still at the loop end — instead of a block of silence.
    #[test]
    fn looped_streamed_clip_wraps_without_a_silent_block() {
        use crate::audio_source::ClipAudioSource;
        use crate::engine::render_project_block_interleaved_with_taps;
        use crate::runtime::RuntimeProject;
        use crate::transport::{advance_loop_position, segment_frames_until_loop_wrap, LoopBounds};

        let frames = 600_000u64;
        let path = write_index_wav("loop", frames);
        let media = path.to_str().unwrap().to_string();
        let stream = Arc::new(StreamingSource::open(&media).expect("open stream"));
        let mut cache = std::collections::HashMap::new();
        cache.insert(
            media.clone(),
            Arc::new(ClipAudioSource::Streaming(Arc::clone(&stream))),
        );
        let mut snapshot = crate::export::silence_snapshot(48_000);
        snapshot.clips.push(crate::types::EngineClipSnapshot {
            id: "looped".to_string(),
            track_id: "track-1".to_string(),
            asset_id: "asset".to_string(),
            media_path: Some(media.clone()),
            start_beat: 0.0,
            duration_beats: 20.0,
            offset_seconds: 0.0,
            gain: 1.0,
            muted: false,
            ara_rendered: false,
            fades: None,
            stretch: Default::default(),
            audio_process: None,
        });
        let mut runtime =
            RuntimeProject::build(&snapshot, 48_000, &mut cache, None, true).expect("runtime");

        // A 1 s loop from 2 s; 48000 is not a multiple of the block, so the
        // wrap falls inside a block, as it usually does.
        let bounds = LoopBounds {
            start: 96_000,
            end: 144_000,
        };
        let block = 256u64;
        let mut out = vec![0.0f32; block as usize * 2];
        let mut pos = bounds.start;
        let mut wraps = 0;
        while wraps < 3 {
            let wraps_here = segment_frames_until_loop_wrap(pos, block, Some(bounds)) < block;
            if wraps_here {
                wait_for("the window to settle", || settled(&stream));
                assert!(
                    !window_covers(&stream, bounds.start, bounds.start + 1),
                    "the wrap must not be served by the window"
                );
                wait_for("the loop cue to fill", || {
                    cue_covers(&stream, StreamCue::Loop, bounds.start, bounds.start + block)
                });
            } else {
                // Stand-in for real-time pacing: the decoder had its time.
                wait_for("the block's audio", || {
                    stream.ring.buffered_frame(pos).is_some()
                        && stream.ring.buffered_frame(pos + block).is_some()
                });
            }
            out.fill(0.0);
            render_project_block_interleaved_with_taps(
                &mut runtime,
                pos,
                1.0,
                &mut out,
                2,
                true,
                4,
                4,
                Some(bounds),
                None,
            );
            let silent = out
                .chunks_exact(2)
                .filter(|frame| frame[0] == 0.0 && frame[1] == 0.0)
                .count();
            assert_eq!(
                silent, 0,
                "the block at {pos} (wrap: {wraps_here}) played {silent} silent frames"
            );
            let (next, wrapped) = advance_loop_position(pos, block, Some(bounds));
            wraps += usize::from(wrapped);
            pos = next;
        }
        assert_eq!(stream.ring.underruns(), 0);
        drop(runtime);
        drop(cache);
        drop(stream);
        let _ = std::fs::remove_file(&path);
    }

    /// #370: an offline stream never answers with the silence of a frame its
    /// decoder has not reached yet. Reads anywhere, at once, wait and return
    /// the right frame.
    #[test]
    fn blocking_reads_wait_for_the_decoder() {
        let frames = 900_000u64;
        let path = write_index_wav("blocking", frames);
        let source = StreamingSource::open(path.to_str().unwrap()).expect("open stream");
        source.set_blocking(true);

        for f in [0u64, 850_000, 123_457, 700_001, 5, frames - 1] {
            assert_eq!(decode_index(source.read_interp(f as f64)), f);
        }
        for f in (300_000..420_000u64).step_by(37) {
            assert_eq!(decode_index(source.read_interp(f as f64)), f);
        }
        assert_eq!(source.ring.underruns(), 0);
        assert!(source.blocking_failure().is_none());
        drop(source);
        let _ = std::fs::remove_file(&path);
    }

    /// #370: an offline read the decoder can no longer serve fails the
    /// render with a reason instead of waiting forever or writing silence.
    #[test]
    fn blocking_read_reports_a_stopped_decoder() {
        let frames = 600_000u64;
        let path = write_index_wav("blocking_stopped", frames);
        let source = StreamingSource::open(path.to_str().unwrap()).expect("open stream");
        source.set_blocking(true);
        source.stop.store(true, Ordering::Release);
        source.wake_worker();
        wait_for("the decoder to stop", || {
            source.ring.worker_exited.load(Ordering::Acquire)
        });

        assert_eq!(source.read_interp(550_000.0), (0.0, 0.0));
        let failure = source
            .blocking_failure()
            .expect("the failed read is reported");
        assert!(failure.contains("550000"), "{failure}");
        drop(source);
        let _ = std::fs::remove_file(&path);
    }
}

//! Stage 2: the lock-free **shared-memory audio bridge** layout between the main
//! `SphereDirectAudioEngine` (in `FutureboardNative.exe`) and the separated
//! `FutureboardPluginHostX64.exe`.
//!
//! The region is a single `#[repr(C)]` POD ([`SharedAudioBridge`]) placed in
//! OS shared memory (a named, pagefile-backed file mapping on Windows). It
//! carries, in one cache-coherent block both processes map:
//!
//! - **audio input buffer** (engine → host, the track's pre-plugin signal),
//! - **audio output buffer** (host → engine, the plugin's processed signal),
//! - **MIDI event ring** (engine → host, SPSC),
//! - **parameter-automation ring** (engine → host, SPSC),
//! - **status / latency / meter block** (atomics, both directions).
//!
//! # Realtime contract (spec Stage 2)
//!
//! Every accessor here is **wait-free**: no heap allocation, no locks, no
//! syscalls, no blocking. Indices are plain atomics; the audio buffers are
//! guarded by the `request_seq` / `done_seq` handshake (a single-producer /
//! single-consumer protocol), so neither side ever waits on the other — the
//! engine publishes a block and reads back whatever the host last produced
//! (one-block latency), it never spins for the host.
//!
//! Stage 2 defines + maps the region and validates the handshake. Wiring it to
//! the audio callback and the plugin `process()` is Stage 3, so the copy/exchange
//! helpers exist but are not yet pumped.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Magic stamped in the header by the creator: `"FBAB"` (Futureboard Audio
/// Bridge), little-endian.
pub const BRIDGE_MAGIC: u32 = 0x4642_4142;
/// Layout version — bump on any change to [`SharedAudioBridge`] field order/size.
/// v2 added the transport / ProcessContext block (tempo, time signature,
/// project position, playing/recording). v3 added VSTi output-channel metadata.
/// v5 expands the shared audio buffers to carry up to 32 plugin output channels.
/// v6 adds the built-in DSP telemetry block (in/out peak+rms, clip flags).
/// v7 adds the analyser spectrum block (log-spaced dB bins + publish sequence)
/// and the `builtin_meters_published` flag that word pair replaced padding for.
/// v8 adds `builtin_gain_reduction` — a compressor's reduction cannot be
/// recovered from the in/out levels, because makeup gain and the dry blend both
/// sit between them.
/// v9 adds the per-stage telemetry block: in/out peak for each of a built-in's
/// rack positions. A multi-stage built-in's internal levels cannot be derived
/// from the strip's endpoints, so an editor that meters its own chain needs
/// them published. Built-ins with no rack publish zeros.
/// v10 adds the stereo-image block: output correlation overall and per band,
/// band levels, and a decimated vectorscope, with its own publish sequence.
/// None of it can be recovered from the strip's peak/RMS words.
/// v11 adds the per-pad level block: a drum sampler's pads each meter what
/// they are playing, which no strip-level word can show.
/// v12 adds the per-band reduction block: a multiband compressor takes a
/// different amount off each band, and the single reduction word can only
/// carry one of them.
pub const BRIDGE_LAYOUT_VERSION: u32 = 12;

/// Rack positions a built-in may publish per-stage telemetry for. Fixed so the
/// shared region stays a plain-old-data layout.
pub const BUILTIN_RACK_SLOTS: usize = 6;

/// Bands a built-in may publish a stereo image for (Imager's crossover bands).
pub const IMAGE_BANDS: usize = 4;
/// Left/right pairs in one published vectorscope frame.
pub const IMAGE_SCOPE_POINTS: usize = 128;

/// Pads a built-in may publish a level for (Drum Sampler's 16).
pub const BUILTIN_PAD_SLOTS: usize = 16;

/// Bands a built-in may publish a gain reduction for (the Compressor's
/// multiband crossover bands).
pub const REDUCTION_BANDS: usize = 4;

/// `transport_flags` bits.
pub const TRANSPORT_FLAG_PLAYING: u32 = 1 << 0;
pub const TRANSPORT_FLAG_RECORDING: u32 = 1 << 1;

/// One telemetry frame from a built-in DSP: linear 0..1 levels measured after
/// the corresponding trim, plus sticky clip latches. Mirrors rodharerist's
/// `MeterFrame` (and the editor's TS `MeterFrame`) field-for-field.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BuiltinMeterFrame {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    /// Decibels the DSP is currently taking off, positive. `0.0` for a
    /// built-in that does not compress — a reduction meter reading zero and a
    /// plugin with no reduction to report look the same, which is correct:
    /// neither is taking anything off.
    pub gain_reduction_db: f32,
    pub in_clip: bool,
    pub out_clip: bool,
    /// Level entering each rack position, in chain order. All zeros for a
    /// built-in that is a single fixed stage rather than a user-ordered rack.
    pub slot_in_peak: [f32; BUILTIN_RACK_SLOTS],
    /// Level leaving each rack position, after that stage's own output trim.
    pub slot_out_peak: [f32; BUILTIN_RACK_SLOTS],
}

/// One stereo-image frame from a built-in that analyses its own output image.
/// Published at the analyser rate (~30 Hz) under its own sequence, like the
/// spectrum — a scope frame is a window of samples, not a per-block level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StereoImageFrame {
    /// Output correlation, −1 (one side inverted) .. +1 (mono). `0.0` while
    /// the output is too quiet to measure.
    pub correlation: f32,
    /// The same, per band, lowest band first.
    pub band_correlation: [f32; IMAGE_BANDS],
    /// Output RMS per band, linear, so a reader can tell an empty band's `0.0`
    /// correlation from a measured one.
    pub band_level: [f32; IMAGE_BANDS],
    /// Decimated output samples as interleaved left/right pairs, oldest first.
    pub scope: [f32; IMAGE_SCOPE_POINTS * 2],
}

impl Default for StereoImageFrame {
    fn default() -> Self {
        Self {
            correlation: 0.0,
            band_correlation: [0.0; IMAGE_BANDS],
            band_level: [0.0; IMAGE_BANDS],
            scope: [0.0; IMAGE_SCOPE_POINTS * 2],
        }
    }
}

/// Maximum block size (frames) the region can carry. The engine's actual block
/// must be `<=` this; the region is sized for the worst case so it never
/// reallocates.
///
/// Defined by the engine, which owns the bridge contract and sizes the device
/// buffer against it. Aliased rather than duplicated so the two cannot drift
/// into a silent truncation.
pub const MAX_BLOCK_FRAMES: usize = DirectAudio::plugin_bridge::MAX_BRIDGE_BLOCK_FRAMES;
/// Channels per audio buffer. The engine still consumes a stereo track today,
/// but the bridge carries multichannel VSTi output-bus data so no plugin
/// output channels are silently dropped before the engine-side downmix.
pub const MAX_CHANNELS: usize = 32;
/// Interleaved samples per audio buffer.
pub const AUDIO_BUF_LEN: usize = MAX_BLOCK_FRAMES * MAX_CHANNELS;

/// MIDI ring capacity (power of two).
pub const MIDI_RING_CAP: usize = 1024;
/// Parameter-automation ring capacity (power of two).
pub const PARAM_RING_CAP: usize = 1024;

/// `dsp_output_state` values.
pub const DSP_OUTPUT_PENDING: u32 = 0;
pub const DSP_OUTPUT_READY: u32 = 1;

/// One MIDI event in the ring (POD, fixed size). `status`/`data1`/`data2` are
/// raw MIDI bytes; `sample_offset` is the frame within the current block.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharedMidiEvent {
    pub sample_offset: u32,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
    pub _pad: u8,
}

/// One parameter-automation point in the ring (POD, fixed size).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SharedParamEvent {
    pub sample_offset: u32,
    pub param_id: u32,
    pub value: f32,
    pub _pad: u32,
}

/// Generic single-producer / single-consumer ring stored inline in shared
/// memory. `head` is the consumer cursor, `tail` the producer cursor; both wrap
/// at `2 * CAP` so the empty/full distinction needs no extra flag (the mask is
/// applied only when indexing the storage).
#[repr(C)]
pub struct SpscRing<T: Copy, const CAP: usize> {
    /// Consumer index (monotonic, wraps at `u32`).
    head: AtomicU32,
    /// Producer index (monotonic, wraps at `u32`).
    tail: AtomicU32,
    /// Backing storage. `UnsafeCell` because the producer writes a slot through a
    /// shared reference; the SPSC index protocol guarantees no slot is read and
    /// written concurrently.
    slots: [UnsafeCell<T>; CAP],
}

// The SPSC index protocol (one producer, one consumer, `Acquire`/`Release`
// publication) makes concurrent access sound; the region itself lives in shared
// memory that both processes treat under the same contract.
unsafe impl<T: Copy + Send, const CAP: usize> Sync for SpscRing<T, CAP> {}

impl<T: Copy + Default, const CAP: usize> SpscRing<T, CAP> {
    const MASK: u32 = (CAP as u32) - 1;

    /// Compile-time guard: `CAP` must be a non-zero power of two.
    const ASSERT_POW2: () = assert!(CAP.is_power_of_two() && CAP > 1);

    /// Producer side: append `value`. Returns `false` (dropping the event) when
    /// the ring is full — wait-free, never blocks the audio thread.
    pub fn try_push(&self, value: T) -> bool {
        let () = Self::ASSERT_POW2;
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= CAP as u32 {
            return false; // full
        }
        let idx = (tail & Self::MASK) as usize;
        // SAFETY: single producer owns `tail`; this slot is not being read
        // because the consumer only reads indices `< tail` (published below).
        unsafe {
            self.slots[idx].get().write(value);
        }
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side: pop the oldest value, or `None` when empty. Wait-free.
    pub fn try_pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head == tail {
            return None; // empty
        }
        let idx = (head & Self::MASK) as usize;
        // SAFETY: single consumer owns `head`; slot was published by the producer
        // before it bumped `tail` (Release/Acquire pair above).
        let value = unsafe { self.slots[idx].get().read() };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    /// Drop every queued value (consumer side). Used on reset / panic.
    pub fn clear(&self) {
        while self.try_pop().is_some() {}
    }

    /// Approximate number of queued items (diagnostics only).
    pub fn len(&self) -> u32 {
        self.tail
            .load(Ordering::Acquire)
            .wrapping_sub(self.head.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Interleaved audio buffer stored inline. Plain `f32` guarded by the
/// `request_seq`/`done_seq` handshake — not atomic, because exactly one side
/// touches it between sequence bumps.
#[repr(C)]
pub struct SharedAudioBuffer {
    samples: [UnsafeCell<f32>; AUDIO_BUF_LEN],
}

unsafe impl Sync for SharedAudioBuffer {}

impl SharedAudioBuffer {
    /// Copy `src` (interleaved, `<= AUDIO_BUF_LEN`) into the buffer. The caller
    /// must own the buffer per the seq handshake (no concurrent reader). Any
    /// samples beyond `src.len()` are zeroed so a later larger read never pulls
    /// a stale wet tail from a previous shorter block.
    ///
    /// # Safety
    /// SPSC contract: only the producing side may call this, and only while it
    /// holds the block (before publishing `request_seq` / after the consumer
    /// released it).
    pub unsafe fn write_interleaved(&self, src: &[f32]) {
        let n = src.len().min(AUDIO_BUF_LEN);
        let dst = self.samples.as_ptr() as *mut f32;
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst, n);
            if n < AUDIO_BUF_LEN {
                std::ptr::write_bytes(dst.add(n), 0, AUDIO_BUF_LEN - n);
            }
        }
    }

    /// Copy `count` interleaved samples into `dst`.
    ///
    /// # Safety
    /// SPSC contract: only the consuming side may call this while it holds the
    /// block.
    pub unsafe fn read_interleaved(&self, dst: &mut [f32], count: usize) {
        let n = count.min(AUDIO_BUF_LEN).min(dst.len());
        let src = self.samples.as_ptr() as *const f32;
        unsafe {
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), n);
        }
    }

    /// Read up to `frames` stereo frames split into `out_l` / `out_r`. Returns
    /// the number of frames written. Wait-free (raw load + arithmetic).
    ///
    /// # Safety
    /// SPSC contract: only the consuming side (engine) may call this while it
    /// holds the block.
    pub unsafe fn read_deinterleaved(
        &self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        frames: usize,
    ) -> usize {
        let n = frames
            .min(MAX_BLOCK_FRAMES)
            .min(out_l.len())
            .min(out_r.len());
        let src = self.samples.as_ptr() as *const f32;
        for i in 0..n {
            unsafe {
                out_l[i] = *src.add(i * 2);
                out_r[i] = *src.add(i * 2 + 1);
            }
        }
        n
    }

    /// Read interleaved plugin output and downmix `channels` to stereo.
    ///
    /// Channel layout is intentionally conservative until the mixer has real
    /// multi-output routing: mono goes center, stereo pairs keep L/R, and any
    /// additional pairs are folded into L/R at equal-power-ish pair gain.
    ///
    /// # Safety
    /// SPSC contract: only the consuming side (engine) may call this while it
    /// holds the block.
    pub unsafe fn read_downmixed_to_stereo(
        &self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        frames: usize,
        channels: usize,
    ) -> usize {
        let channels = channels.clamp(1, MAX_CHANNELS);
        let n = frames
            .min(MAX_BLOCK_FRAMES)
            .min(out_l.len())
            .min(out_r.len());
        let src = self.samples.as_ptr() as *const f32;
        let extra_pair_gain = 0.70710677f32;
        for i in 0..n {
            let base = i * channels;
            let mut l = unsafe { *src.add(base) };
            let mut r = if channels > 1 {
                unsafe { *src.add(base + 1) }
            } else {
                l
            };
            let mut ch = 2usize;
            while ch < channels {
                let extra_l = unsafe { *src.add(base + ch) };
                let extra_r = if ch + 1 < channels {
                    unsafe { *src.add(base + ch + 1) }
                } else {
                    extra_l
                };
                l += extra_l * extra_pair_gain;
                r += extra_r * extra_pair_gain;
                ch += 2;
            }
            out_l[i] = l;
            out_r[i] = r;
        }
        n
    }

    /// Read interleaved plugin output and downmix selected 1-based output
    /// channels to stereo. Empty selection folds all reported channels; this
    /// keeps legacy/unsynced multi-out instruments audible instead of dropping
    /// routed aux buses until the UI selection reaches the engine.
    ///
    /// # Safety
    /// SPSC contract: only the consuming side (engine) may call this while it
    /// holds the block.
    pub unsafe fn read_downmixed_to_stereo_selected(
        &self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        frames: usize,
        channels: usize,
        enabled_channels: &[u8],
    ) -> usize {
        let channels = channels.clamp(1, MAX_CHANNELS);
        if enabled_channels.is_empty() {
            return unsafe { self.read_downmixed_to_stereo(out_l, out_r, frames, channels) };
        }

        let n = frames
            .min(MAX_BLOCK_FRAMES)
            .min(out_l.len())
            .min(out_r.len());
        let src = self.samples.as_ptr() as *const f32;
        let extra_pair_gain = 0.70710677f32;
        for i in 0..n {
            let base = i * channels;
            let mut l = 0.0f32;
            let mut r = 0.0f32;
            for &channel in enabled_channels {
                let ch = channel as usize;
                if ch == 0 || ch > channels || ch > MAX_CHANNELS {
                    continue;
                }
                let sample = unsafe { *src.add(base + ch - 1) };
                match ch {
                    1 => l += sample,
                    2 => r += sample,
                    _ if ch % 2 == 1 => l += sample * extra_pair_gain,
                    _ => r += sample * extra_pair_gain,
                }
            }
            out_l[i] = l;
            out_r[i] = r;
        }
        n
    }

    /// Read interleaved plugin output, fold selected 1-based channels to
    /// stereo, and compute per-output peak values for the same fresh block.
    ///
    /// # Safety
    /// SPSC contract: only the consuming side (engine) may call this while it
    /// holds the block.
    pub unsafe fn read_downmixed_to_stereo_selected_with_peaks(
        &self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        frames: usize,
        channels: usize,
        enabled_channels: &[u8],
        peaks: &mut [f32; MAX_CHANNELS],
    ) -> usize {
        peaks.fill(0.0);
        let channels = channels.clamp(1, MAX_CHANNELS);
        let n = frames
            .min(MAX_BLOCK_FRAMES)
            .min(out_l.len())
            .min(out_r.len());
        let src = self.samples.as_ptr() as *const f32;
        let extra_pair_gain = 0.70710677f32;
        for i in 0..n {
            let base = i * channels;
            // `ch_ix` is a raw-pointer offset into the interleaved block, not
            // merely a `peaks` index — the explicit range loop is intentional.
            #[allow(clippy::needless_range_loop)]
            for ch_ix in 0..channels {
                let sample = unsafe { *src.add(base + ch_ix) };
                peaks[ch_ix] = peaks[ch_ix].max(sample.abs());
            }

            if enabled_channels.is_empty() {
                let mut l = unsafe { *src.add(base) };
                let mut r = if channels > 1 {
                    unsafe { *src.add(base + 1) }
                } else {
                    l
                };
                let mut ch = 2usize;
                while ch < channels {
                    let extra_l = unsafe { *src.add(base + ch) };
                    let extra_r = if ch + 1 < channels {
                        unsafe { *src.add(base + ch + 1) }
                    } else {
                        extra_l
                    };
                    l += extra_l * extra_pair_gain;
                    r += extra_r * extra_pair_gain;
                    ch += 2;
                }
                out_l[i] = l;
                out_r[i] = r;
                continue;
            }

            let mut l = 0.0f32;
            let mut r = 0.0f32;
            for &channel in enabled_channels {
                let ch = channel as usize;
                if ch == 0 || ch > channels || ch > MAX_CHANNELS {
                    continue;
                }
                let sample = unsafe { *src.add(base + ch - 1) };
                match ch {
                    1 => l += sample,
                    2 => r += sample,
                    _ if ch % 2 == 1 => l += sample * extra_pair_gain,
                    _ => r += sample * extra_pair_gain,
                }
            }
            out_l[i] = l;
            out_r[i] = r;
        }
        n
    }

    /// Copy deinterleaved stereo into the buffer (engine → host `audio_in`).
    ///
    /// # Safety
    /// SPSC contract: only the producing side (engine) may call this while it
    /// holds the block.
    pub unsafe fn write_deinterleaved(&self, in_l: &[f32], in_r: &[f32], frames: usize) {
        let n = frames.min(MAX_BLOCK_FRAMES).min(in_l.len()).min(in_r.len());
        let dst = self.samples.as_ptr() as *mut f32;
        for i in 0..n {
            unsafe {
                *dst.add(i * 2) = in_l[i];
                *dst.add(i * 2 + 1) = in_r[i];
            }
        }
        // Clear any leftover stereo frames so a later larger block_frames never
        // re-reads a stale dry tail from a shorter write.
        let clear_from = n * 2;
        let clear_len = (MAX_BLOCK_FRAMES * 2)
            .saturating_sub(clear_from)
            .min(AUDIO_BUF_LEN.saturating_sub(clear_from));
        if clear_len > 0 {
            unsafe {
                std::ptr::write_bytes(dst.add(clear_from), 0, clear_len);
            }
        }
    }
}

/// The full shared region. Created (zeroed) by the OS mapping; the creator
/// stamps the header. Field order is the wire layout — do not reorder without
/// bumping [`BRIDGE_LAYOUT_VERSION`].
#[repr(C)]
pub struct SharedAudioBridge {
    // --- Header (creator-stamped, read-only afterwards) ---
    pub magic: AtomicU32,
    pub layout_version: AtomicU32,
    pub sample_rate: AtomicU32,
    pub max_block_size: AtomicU32,
    pub in_channels: AtomicU32,
    pub out_channels: AtomicU32,
    /// Actual main audio output channels reported by the loaded plugin. This is
    /// metadata for routing; `out_channels` above still describes the current
    /// shared-buffer layout.
    pub plugin_output_channels: AtomicU32,
    /// Host DSP blocks whose process time met or exceeded the block deadline.
    /// This occupies the former header padding slot, preserving the wire size.
    pub producer_deadline_misses: AtomicU32,

    // --- Block handshake (lock-free, one-block latency) ---
    /// Bumped by the engine after it writes `audio_in` + the rings for a block.
    pub request_seq: AtomicU64,
    /// Bumped by the host after it writes `audio_out` for that block.
    pub done_seq: AtomicU64,
    /// Frames in the current block (`<= max_block_size`).
    pub block_frames: AtomicU32,
    /// Duration of the most recently completed host DSP block in microseconds.
    /// Reuses the old padding slot, so the shared-memory ABI size is unchanged.
    pub last_process_micros: AtomicU32,

    // --- Status / latency / meters (atomics, both directions) ---
    /// [`DSP_OUTPUT_PENDING`] / [`DSP_OUTPUT_READY`].
    pub dsp_output_state: AtomicU32,
    /// Plugin reported latency in samples (Stage 4).
    pub latency_samples: AtomicU32,
    /// Output peak meters, `f32` bits (host → engine).
    pub meter_peak_l: AtomicU32,
    pub meter_peak_r: AtomicU32,
    /// Count of dropped/late blocks (diagnostics).
    pub xrun_count: AtomicU32,
    /// Longest host DSP block observed since this region was created.
    /// Reuses the old padding slot, so the shared-memory ABI size is unchanged.
    pub max_process_micros: AtomicU32,

    // --- Transport / ProcessContext (engine → host, atomics) ---
    // Published by the engine each block alongside `request_seq`; read by the
    // host producer to fill the plugin's VST3 ProcessContext before process().
    /// Tempo in quarter-notes/min, `f64` bits.
    pub transport_tempo_bits: AtomicU64,
    /// Project position in quarter notes (`projectTimeMusic`), `f64` bits.
    pub transport_ppq_bits: AtomicU64,
    /// Current bar-start position in quarter notes (`barPositionMusic`), `f64` bits.
    pub transport_bar_ppq_bits: AtomicU64,
    /// Absolute project sample position of the block start (`i64` as bits).
    pub transport_project_samples: AtomicU64,
    /// `(num << 16) | den` time signature.
    pub transport_time_sig: AtomicU32,
    /// [`TRANSPORT_FLAG_PLAYING`] / [`TRANSPORT_FLAG_RECORDING`].
    pub transport_flags: AtomicU32,

    // --- Built-in DSP telemetry (host → engine, f32 bits / flags) ---
    // Written per block by the host producer for built-in plugins (from the
    // DSP's own `MeterFrame`); zero for external VST3 instances. Levels are
    // linear 0..1 measured after the corresponding trim; clip flags are
    // sticky in the DSP until it receives a `clear_clip` param event.
    pub builtin_in_peak: AtomicU32,
    pub builtin_in_rms: AtomicU32,
    pub builtin_out_peak: AtomicU32,
    pub builtin_out_rms: AtomicU32,
    /// Gain reduction in dB (positive), `f32` bits. Published by compressors;
    /// stays zero for built-ins with nothing to reduce.
    pub builtin_gain_reduction: AtomicU32,
    /// Bit 0: input clip latched; bit 1: output clip latched.
    pub builtin_clip_flags: AtomicU32,
    /// Non-zero once the host has published at least one telemetry frame for
    /// this instance. Distinguishes "this DSP does not measure anything" (an
    /// EQ has no meters) from "measured, and the signal is silent" — without
    /// it a reader cannot tell the two apart, since both read as all zeros.
    pub builtin_meters_published: AtomicU32,
    /// Per-rack-position in/out peak, `f32` bits, in chain order. Written in
    /// the same store as the block above, so a reader that sees a fresh frame
    /// sees these too. All zeros for a built-in with no user-ordered rack.
    pub builtin_slot_in_peak: [AtomicU32; BUILTIN_RACK_SLOTS],
    pub builtin_slot_out_peak: [AtomicU32; BUILTIN_RACK_SLOTS],

    // --- Analyser spectrum (host → engine) ---
    /// Log-spaced magnitude bins in dB, `f32` bits, published by the host at
    /// the analyser's own rate (~30 Hz) rather than per block. See
    /// [`crate::spectrum`] for the frequency map and level scale.
    pub spectrum_bins: [AtomicU32; crate::spectrum::SPECTRUM_BINS],
    /// Bumped after each published frame; `0` until the first one. A reader
    /// uses it both to skip republishing a frame it already sent and to tell
    /// "this insert publishes no spectrum" from "the spectrum is silent".
    pub spectrum_seq: AtomicU32,
    pub _pad_spectrum: AtomicU32,

    // --- Stereo image (host → engine) ---
    /// [`StereoImageFrame`] fields, `f32` bits, published at the analyser rate
    /// by a built-in that measures its own image. See [`Self::store_stereo_image`].
    pub image_correlation: AtomicU32,
    pub image_band_correlation: [AtomicU32; IMAGE_BANDS],
    pub image_band_level: [AtomicU32; IMAGE_BANDS],
    pub image_scope: [AtomicU32; IMAGE_SCOPE_POINTS * 2],
    /// Bumped after each published frame; `0` until the first one, which is
    /// how a reader tells "this insert measures no image" from silence.
    pub image_seq: AtomicU32,
    pub _pad_image: AtomicU32,

    // --- Per-pad levels (host → engine) ---
    /// Held peak of each pad's output, linear, `f32` bits. Published per
    /// block by a built-in with pads; see [`Self::store_pad_levels`].
    pub pad_levels: [AtomicU32; BUILTIN_PAD_SLOTS],
    /// Bumped after each published set; `0` until the first one.
    pub pad_levels_seq: AtomicU32,
    pub _pad_pad_levels: AtomicU32,

    // --- Per-band gain reduction (host → engine) ---
    /// Decibels each band is taking off, positive, `f32` bits. Published per
    /// block by a multiband built-in; see [`Self::store_band_reduction`].
    pub band_reduction: [AtomicU32; REDUCTION_BANDS],
    /// Bumped after each published set; `0` until the first one.
    pub band_reduction_seq: AtomicU32,
    pub _pad_band_reduction: AtomicU32,

    // --- Lock-free rings (engine → host) ---
    pub midi: SpscRing<SharedMidiEvent, MIDI_RING_CAP>,
    pub params: SpscRing<SharedParamEvent, PARAM_RING_CAP>,

    // --- Audio buffers ---
    pub audio_in: SharedAudioBuffer,
    pub audio_out: SharedAudioBuffer,
}

impl SharedAudioBridge {
    /// Total region size in bytes.
    pub const SIZE: usize = std::mem::size_of::<Self>();

    /// Stamp the header (creator side) and zero the dynamic state. Called once,
    /// right after the zeroed region is mapped.
    pub fn init_header(&self, sample_rate: u32, max_block_size: u32, channels: u32) {
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        self.max_block_size.store(
            max_block_size.min(MAX_BLOCK_FRAMES as u32),
            Ordering::Relaxed,
        );
        self.in_channels.store(channels, Ordering::Relaxed);
        self.out_channels.store(channels, Ordering::Relaxed);
        self.plugin_output_channels
            .store(channels.max(1), Ordering::Relaxed);
        self.producer_deadline_misses.store(0, Ordering::Relaxed);
        self.layout_version
            .store(BRIDGE_LAYOUT_VERSION, Ordering::Relaxed);
        self.dsp_output_state
            .store(DSP_OUTPUT_PENDING, Ordering::Relaxed);
        // Sane transport defaults until the engine publishes the first block, so
        // a host reading early sees 120 BPM / 4-4 / stopped rather than zeros.
        self.transport_tempo_bits
            .store(120.0f64.to_bits(), Ordering::Relaxed);
        self.transport_ppq_bits.store(0, Ordering::Relaxed);
        self.transport_bar_ppq_bits.store(0, Ordering::Relaxed);
        self.transport_project_samples.store(0, Ordering::Relaxed);
        self.transport_time_sig
            .store((4 << 16) | 4, Ordering::Relaxed);
        self.transport_flags.store(0, Ordering::Relaxed);
        // Publish magic last (Release) so an opener that sees the magic also sees
        // a fully-stamped header.
        self.magic.store(BRIDGE_MAGIC, Ordering::Release);
    }

    /// Opener-side validation: the header magic + layout version match.
    pub fn header_valid(&self) -> bool {
        self.magic.load(Ordering::Acquire) == BRIDGE_MAGIC
            && self.layout_version.load(Ordering::Relaxed) == BRIDGE_LAYOUT_VERSION
    }

    pub fn set_dsp_output_ready(&self, ready: bool) {
        self.dsp_output_state.store(
            if ready {
                DSP_OUTPUT_READY
            } else {
                DSP_OUTPUT_PENDING
            },
            Ordering::Release,
        );
    }

    pub fn dsp_output_ready(&self) -> bool {
        self.dsp_output_state.load(Ordering::Acquire) == DSP_OUTPUT_READY
    }

    pub fn set_plugin_output_channels(&self, channels: u32) {
        self.plugin_output_channels
            .store(channels.max(1), Ordering::Release);
    }

    pub fn plugin_output_channels(&self) -> u32 {
        self.plugin_output_channels.load(Ordering::Acquire).max(1)
    }

    /// Publish one producer DSP duration and classify it against the exact
    /// sample-rate/block-size deadline. Atomics only; safe on the producer hot
    /// path and observable from the control/UI diagnostics thread.
    pub fn record_process_timing(&self, frames: usize, process_micros: u32) {
        self.last_process_micros
            .store(process_micros, Ordering::Relaxed);
        self.max_process_micros
            .fetch_max(process_micros, Ordering::Relaxed);
        let sample_rate = self.sample_rate.load(Ordering::Relaxed).max(1) as u64;
        let deadline_micros = (frames as u64)
            .saturating_mul(1_000_000)
            .saturating_add(sample_rate - 1)
            / sample_rate;
        if deadline_micros > 0 && process_micros as u64 >= deadline_micros {
            self.producer_deadline_misses
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Store the host output peak meters (host side).
    pub fn store_meters(&self, peak_l: f32, peak_r: f32) {
        self.meter_peak_l.store(peak_l.to_bits(), Ordering::Relaxed);
        self.meter_peak_r.store(peak_r.to_bits(), Ordering::Relaxed);
    }

    /// Read the host output peak meters (engine side).
    pub fn meters(&self) -> (f32, f32) {
        (
            f32::from_bits(self.meter_peak_l.load(Ordering::Relaxed)),
            f32::from_bits(self.meter_peak_r.load(Ordering::Relaxed)),
        )
    }

    /// Publish a built-in DSP's telemetry frame (host producer, per block).
    pub fn store_builtin_meters(&self, frame: &BuiltinMeterFrame) {
        self.builtin_in_peak
            .store(frame.in_peak.to_bits(), Ordering::Relaxed);
        self.builtin_in_rms
            .store(frame.in_rms.to_bits(), Ordering::Relaxed);
        self.builtin_out_peak
            .store(frame.out_peak.to_bits(), Ordering::Relaxed);
        self.builtin_out_rms
            .store(frame.out_rms.to_bits(), Ordering::Relaxed);
        self.builtin_gain_reduction
            .store(frame.gain_reduction_db.to_bits(), Ordering::Relaxed);
        let mut flags = 0u32;
        if frame.in_clip {
            flags |= 1;
        }
        if frame.out_clip {
            flags |= 2;
        }
        self.builtin_clip_flags.store(flags, Ordering::Relaxed);
        for (slot, value) in self.builtin_slot_in_peak.iter().zip(frame.slot_in_peak) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        for (slot, value) in self.builtin_slot_out_peak.iter().zip(frame.slot_out_peak) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        // Release: a reader that observes the flag must also see the level
        // words and per-stage arrays stored above.
        self.builtin_meters_published.store(1, Ordering::Release);
    }

    /// Read the built-in DSP's telemetry frame (engine/UI side). `None` until
    /// the host has published one — a built-in with no metering never does, and
    /// its readers must not mistake the zeroed region for a measured silence.
    pub fn builtin_meters(&self) -> Option<BuiltinMeterFrame> {
        if self.builtin_meters_published.load(Ordering::Acquire) == 0 {
            return None;
        }
        let flags = self.builtin_clip_flags.load(Ordering::Relaxed);
        Some(BuiltinMeterFrame {
            in_peak: f32::from_bits(self.builtin_in_peak.load(Ordering::Relaxed)),
            in_rms: f32::from_bits(self.builtin_in_rms.load(Ordering::Relaxed)),
            out_peak: f32::from_bits(self.builtin_out_peak.load(Ordering::Relaxed)),
            out_rms: f32::from_bits(self.builtin_out_rms.load(Ordering::Relaxed)),
            gain_reduction_db: f32::from_bits(self.builtin_gain_reduction.load(Ordering::Relaxed)),
            in_clip: flags & 1 != 0,
            out_clip: flags & 2 != 0,
            slot_in_peak: std::array::from_fn(|i| {
                f32::from_bits(self.builtin_slot_in_peak[i].load(Ordering::Relaxed))
            }),
            slot_out_peak: std::array::from_fn(|i| {
                f32::from_bits(self.builtin_slot_out_peak[i].load(Ordering::Relaxed))
            }),
        })
    }

    /// Publish one analyser frame (host producer, ~30 Hz).
    ///
    /// Deliberately **not** a seqlock. A reader that catches this mid-write
    /// gets a frame whose low bins are one analysis newer than its high bins —
    /// invisible at 30 Hz, and harmless because these numbers only ever drive a
    /// visual overlay. Nothing in the audio path, automation, or persisted
    /// state reads them, so paying for a retry loop on the producer thread
    /// would buy nothing.
    pub fn store_spectrum(&self, bins: &[f32; crate::spectrum::SPECTRUM_BINS]) {
        for (slot, db) in self.spectrum_bins.iter().zip(bins) {
            slot.store(db.to_bits(), Ordering::Relaxed);
        }
        // Release: a reader that observes the new sequence also sees the bins.
        self.spectrum_seq.fetch_add(1, Ordering::Release);
    }

    /// Read the latest analyser frame with the sequence it was published under.
    /// `None` until the host has published one — an insert whose DSP feeds no
    /// analyser never does, and the zeroed region must not be mistaken for a
    /// measured full-scale spectrum (`0.0` dB bits are the *top* of the scale,
    /// not the floor).
    pub fn spectrum(&self) -> Option<(u32, [f32; crate::spectrum::SPECTRUM_BINS])> {
        let seq = self.spectrum_seq.load(Ordering::Acquire);
        if seq == 0 {
            return None;
        }
        let mut bins = [0.0f32; crate::spectrum::SPECTRUM_BINS];
        for (out, slot) in bins.iter_mut().zip(self.spectrum_bins.iter()) {
            *out = f32::from_bits(slot.load(Ordering::Relaxed));
        }
        Some((seq, bins))
    }

    /// Publish one stereo-image frame (host producer, ~30 Hz). Not a seqlock,
    /// for the reason [`Self::store_spectrum`] gives: a torn frame is one
    /// analysis mixed with the next, and only ever drives a display.
    pub fn store_stereo_image(&self, frame: &StereoImageFrame) {
        self.image_correlation
            .store(frame.correlation.to_bits(), Ordering::Relaxed);
        for (slot, value) in self
            .image_band_correlation
            .iter()
            .zip(frame.band_correlation)
        {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        for (slot, value) in self.image_band_level.iter().zip(frame.band_level) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        for (slot, value) in self.image_scope.iter().zip(frame.scope) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        // Release: a reader that observes the new sequence also sees the frame.
        self.image_seq.fetch_add(1, Ordering::Release);
    }

    /// Read the latest stereo-image frame with the sequence it was published
    /// under. `None` until the host has published one.
    pub fn stereo_image(&self) -> Option<(u32, StereoImageFrame)> {
        let seq = self.image_seq.load(Ordering::Acquire);
        if seq == 0 {
            return None;
        }
        let load = |slot: &AtomicU32| f32::from_bits(slot.load(Ordering::Relaxed));
        Some((
            seq,
            StereoImageFrame {
                correlation: load(&self.image_correlation),
                band_correlation: std::array::from_fn(|i| load(&self.image_band_correlation[i])),
                band_level: std::array::from_fn(|i| load(&self.image_band_level[i])),
                scope: std::array::from_fn(|i| load(&self.image_scope[i])),
            },
        ))
    }

    /// Publish one set of per-pad levels (host producer, per block). Not a
    /// seqlock for the same reason [`Self::store_spectrum`] gives.
    pub fn store_pad_levels(&self, levels: &[f32; BUILTIN_PAD_SLOTS]) {
        for (slot, value) in self.pad_levels.iter().zip(levels) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        // Release: a reader that observes the new sequence also sees the levels.
        self.pad_levels_seq.fetch_add(1, Ordering::Release);
    }

    /// Read the latest per-pad levels with their sequence. `None` until the
    /// host has published one — an insert without pads never does.
    pub fn pad_levels(&self) -> Option<(u32, [f32; BUILTIN_PAD_SLOTS])> {
        let seq = self.pad_levels_seq.load(Ordering::Acquire);
        if seq == 0 {
            return None;
        }
        Some((
            seq,
            std::array::from_fn(|i| f32::from_bits(self.pad_levels[i].load(Ordering::Relaxed))),
        ))
    }

    /// Publish one set of per-band reductions (host producer, per block). Not
    /// a seqlock for the same reason [`Self::store_spectrum`] gives.
    pub fn store_band_reduction(&self, reduction_db: &[f32; REDUCTION_BANDS]) {
        for (slot, value) in self.band_reduction.iter().zip(reduction_db) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        // Release: a reader that observes the new sequence also sees the values.
        self.band_reduction_seq.fetch_add(1, Ordering::Release);
    }

    /// Read the latest per-band reductions with their sequence. `None` until
    /// the host has published one — an insert without bands never does.
    pub fn band_reduction(&self) -> Option<(u32, [f32; REDUCTION_BANDS])> {
        let seq = self.band_reduction_seq.load(Ordering::Acquire);
        if seq == 0 {
            return None;
        }
        Some((
            seq,
            std::array::from_fn(|i| f32::from_bits(self.band_reduction[i].load(Ordering::Relaxed))),
        ))
    }

    /// Publish the transport ProcessContext for the next block (engine side).
    /// Wait-free: plain atomic stores, safe on the audio callback.
    pub fn store_transport(&self, t: &BridgeTransport) {
        self.transport_tempo_bits
            .store(t.tempo_bpm.to_bits(), Ordering::Relaxed);
        self.transport_ppq_bits
            .store(t.ppq_position.to_bits(), Ordering::Relaxed);
        self.transport_bar_ppq_bits
            .store(t.bar_position_ppq.to_bits(), Ordering::Relaxed);
        self.transport_project_samples
            .store(t.project_time_samples as u64, Ordering::Relaxed);
        self.transport_time_sig.store(
            ((t.time_sig_num & 0xFFFF) << 16) | (t.time_sig_den & 0xFFFF),
            Ordering::Relaxed,
        );
        let mut flags = 0u32;
        if t.playing {
            flags |= TRANSPORT_FLAG_PLAYING;
        }
        if t.recording {
            flags |= TRANSPORT_FLAG_RECORDING;
        }
        self.transport_flags.store(flags, Ordering::Relaxed);
    }

    /// Read the transport ProcessContext for this block (host side).
    pub fn load_transport(&self) -> BridgeTransport {
        let time_sig = self.transport_time_sig.load(Ordering::Relaxed);
        let flags = self.transport_flags.load(Ordering::Relaxed);
        BridgeTransport {
            tempo_bpm: f64::from_bits(self.transport_tempo_bits.load(Ordering::Relaxed)),
            time_sig_num: time_sig >> 16,
            time_sig_den: time_sig & 0xFFFF,
            project_time_samples: self.transport_project_samples.load(Ordering::Relaxed) as i64,
            ppq_position: f64::from_bits(self.transport_ppq_bits.load(Ordering::Relaxed)),
            bar_position_ppq: f64::from_bits(self.transport_bar_ppq_bits.load(Ordering::Relaxed)),
            playing: flags & TRANSPORT_FLAG_PLAYING != 0,
            recording: flags & TRANSPORT_FLAG_RECORDING != 0,
        }
    }
}

/// Transport snapshot exchanged through the shared region (engine → host).
/// Mirrors `DirectAudio::RuntimeTransportContext` but lives here so the bridge crate
/// has no dependency direction problem; the host maps it onto the plugin's VST3
/// ProcessContext.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BridgeTransport {
    pub tempo_bpm: f64,
    pub time_sig_num: u32,
    pub time_sig_den: u32,
    pub project_time_samples: i64,
    pub ppq_position: f64,
    pub bar_position_ppq: f64,
    pub playing: bool,
    pub recording: bool,
}

// ---------------------------------------------------------------------------
// Cross-process region handle.
// ---------------------------------------------------------------------------

/// Owns a mapped [`SharedAudioBridge`] region plus its backing (an OS file
/// mapping, or a heap allocation for tests / single-process use). Deref-style
/// access via [`Self::bridge`]. `Send`/`Sync` because the bridge enforces its
/// own realtime SPSC contract.
pub struct SharedAudioRegion {
    ptr: *mut SharedAudioBridge,
    backing: Backing,
}

unsafe impl Send for SharedAudioRegion {}
unsafe impl Sync for SharedAudioRegion {}

enum Backing {
    /// Heap-allocated (aligned, zeroed) — tests and in-process fallback.
    Heap(std::alloc::Layout),
    /// OS file mapping; held only to keep the view alive (unmapped on drop).
    #[cfg(windows)]
    Mapping(#[allow(dead_code)] imp::WinMapping),
    /// POSIX shared-memory mapping; the creator unlinks its name on drop.
    #[cfg(unix)]
    PosixMapping(#[allow(dead_code)] imp::UnixMapping),
}

impl SharedAudioRegion {
    /// Borrow the mapped bridge. Valid for the lifetime of the region.
    pub fn bridge(&self) -> &SharedAudioBridge {
        // SAFETY: `ptr` points at a live, correctly-sized mapping owned by
        // `backing` for the lifetime of `self`.
        unsafe { &*self.ptr }
    }

    /// Region byte size.
    pub fn bytes(&self) -> u64 {
        SharedAudioBridge::SIZE as u64
    }

    /// Allocate an in-process, zeroed region (tests / single-process fallback).
    /// The header is left blank; call [`SharedAudioBridge::init_header`].
    pub fn new_in_process() -> Self {
        let layout = std::alloc::Layout::new::<SharedAudioBridge>();
        // SAFETY: non-zero layout; `alloc_zeroed` yields a properly-aligned,
        // zero-initialized block, which is a valid all-atomics-zero
        // `SharedAudioBridge` (magic 0 = "not yet stamped").
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) as *mut SharedAudioBridge };
        assert!(!ptr.is_null(), "audio bridge allocation failed");
        Self {
            ptr,
            backing: Backing::Heap(layout),
        }
    }

    /// Create a **named** shared region (engine side). Stamps the header.
    #[cfg(any(windows, unix))]
    pub fn create_named(
        name: &str,
        sample_rate: u32,
        max_block_size: u32,
        channels: u32,
    ) -> std::io::Result<Self> {
        #[cfg(windows)]
        let mapping = imp::WinMapping::create(name, SharedAudioBridge::SIZE)?;
        #[cfg(unix)]
        let mapping = imp::UnixMapping::create(name, SharedAudioBridge::SIZE)?;
        let region = Self {
            ptr: mapping.ptr() as *mut SharedAudioBridge,
            #[cfg(windows)]
            backing: Backing::Mapping(mapping),
            #[cfg(unix)]
            backing: Backing::PosixMapping(mapping),
        };
        region
            .bridge()
            .init_header(sample_rate, max_block_size, channels);
        Ok(region)
    }

    /// Open an existing **named** shared region (host side). Validates the header.
    #[cfg(any(windows, unix))]
    pub fn open_named(name: &str) -> std::io::Result<Self> {
        #[cfg(windows)]
        let mapping = imp::WinMapping::open(name, SharedAudioBridge::SIZE)?;
        #[cfg(unix)]
        let mapping = imp::UnixMapping::open(name, SharedAudioBridge::SIZE)?;
        let region = Self {
            ptr: mapping.ptr() as *mut SharedAudioBridge,
            #[cfg(windows)]
            backing: Backing::Mapping(mapping),
            #[cfg(unix)]
            backing: Backing::PosixMapping(mapping),
        };
        if !region.bridge().header_valid() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "shared audio bridge header invalid (magic/version mismatch)",
            ));
        }
        Ok(region)
    }
}

impl Drop for SharedAudioRegion {
    fn drop(&mut self) {
        match &self.backing {
            Backing::Heap(layout) => unsafe {
                std::alloc::dealloc(self.ptr as *mut u8, *layout);
            },
            #[cfg(windows)]
            Backing::Mapping(_) => { /* WinMapping::Drop unmaps + closes handles */ }
            #[cfg(unix)]
            Backing::PosixMapping(_) => { /* UnixMapping::Drop unmaps + unlinks owner */ }
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-process producer wake event.
// ---------------------------------------------------------------------------

/// Name of the named auto-reset event the engine signals after every
/// `request_seq` bump so the host's audio producer wakes immediately instead of
/// sleeping on a timer tick. Scoped to one engine/host **process pair** so a
/// second host process (e.g. a legacy editor-window spawn) can never steal
/// wakeups.
pub fn bridge_kick_event_name(engine_pid: u32, host_pid: u32) -> String {
    format!("Local\\FutureboardAudioBridgeKick-{engine_pid}__{host_pid}")
}

/// Named cross-process wake event for the block handshake.
///
/// Both sides call [`BridgeKickEvent::create_named`], so engine and host may
/// initialize in either order. The engine-side sink calls
/// [`BridgeKickEvent::set`] from the audio callback and the host producer
/// blocks in [`BridgeKickEvent::wait`] until a block is requested.
#[cfg(windows)]
pub struct BridgeKickEvent {
    handle: windows::Win32::Foundation::HANDLE,
}

// SAFETY: an event HANDLE is a kernel object reference; Set/Wait are
// thread-safe by definition.
#[cfg(windows)]
unsafe impl Send for BridgeKickEvent {}
#[cfg(windows)]
unsafe impl Sync for BridgeKickEvent {}

#[cfg(windows)]
impl BridgeKickEvent {
    /// Create (or open, if the peer created it first) the named auto-reset
    /// event. Initially unsignaled.
    pub fn create_named(name: &str) -> std::io::Result<Self> {
        use windows::core::HSTRING;
        use windows::Win32::System::Threading::CreateEventW;
        let wide = HSTRING::from(name);
        let handle =
            unsafe { CreateEventW(None, false, false, &wide) }.map_err(std::io::Error::other)?;
        Ok(Self { handle })
    }

    /// Signal the producer. Wait-free from the caller's perspective — `SetEvent`
    /// never blocks, so this is safe on the engine's audio callback.
    pub fn set(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.handle);
        }
    }

    /// Block until signaled or `timeout_ms` elapses. Returns `true` when the
    /// event was signaled (auto-reset consumes the signal).
    pub fn wait(&self, timeout_ms: u32) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::WaitForSingleObject;
        unsafe { WaitForSingleObject(self.handle, timeout_ms) == WAIT_OBJECT_0 }
    }
}

#[cfg(windows)]
impl Drop for BridgeKickEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(unix)]
pub struct BridgeKickEvent {
    semaphore: *mut libc::sem_t,
    name: std::ffi::CString,
    owner: bool,
}

#[cfg(unix)]
unsafe impl Send for BridgeKickEvent {}
#[cfg(unix)]
unsafe impl Sync for BridgeKickEvent {}

/// The type an IPC creation mode has to be passed as.
///
/// Apple declares `sem_open`/`shm_open` variadic and its `mode_t` is `u16`, so
/// default argument promotion rejects anything narrower than `c_uint`. Every
/// other Unix takes a typed `mode_t` parameter.
#[cfg(all(unix, target_vendor = "apple"))]
type IpcMode = libc::c_uint;
#[cfg(all(unix, not(target_vendor = "apple")))]
type IpcMode = libc::mode_t;

/// Owner read/write — the only access an IPC object shared between the Studio
/// and its own helper process needs.
#[cfg(unix)]
const IPC_OWNER_RW: IpcMode = (libc::S_IRUSR | libc::S_IWUSR) as IpcMode;

/// How long the Unix kick event sleeps between polls of its semaphore.
///
/// macOS implements POSIX named semaphores only partially: `sem_timedwait` and
/// `sem_getvalue` are absent, so there is no blocking wait with a deadline to
/// call. Polling `sem_trywait` on this interval is what the host's producer
/// thread already falls back to when no kick event exists at all
/// (`futureboard_plugin_host`'s `None` arm), so the pacing is not a new one.
/// At the 1 ms timeout that thread uses this costs at most a handful of polls
/// per block.
#[cfg(unix)]
const KICK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_micros(250);

/// Upper bound on posts drained in one wait. The count is kept at one by
/// [`BridgeKickEvent::set`]; this only stops a burst of concurrent setters from
/// turning the drain into an unbounded spin.
#[cfg(unix)]
const KICK_MAX_DRAIN: u32 = 1_024;

#[cfg(unix)]
impl BridgeKickEvent {
    pub fn create_named(name: &str) -> std::io::Result<Self> {
        let name = imp::posix_name("kick", name)?;
        let created = unsafe {
            libc::sem_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL,
                IPC_OWNER_RW,
                0 as libc::c_uint,
            )
        };
        if created != libc::SEM_FAILED {
            return Ok(Self {
                semaphore: created,
                name,
                owner: true,
            });
        }
        let create_error = std::io::Error::last_os_error();
        if create_error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(create_error);
        }
        let semaphore = unsafe { libc::sem_open(name.as_ptr(), 0) };
        if semaphore == libc::SEM_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            semaphore,
            name,
            owner: false,
        })
    }

    /// Signal the producer. Wait-free enough for the engine's audio callback:
    /// two non-blocking semaphore operations, no allocation and no lock.
    ///
    /// Coalesces repeated requests like a Windows auto-reset event, but without
    /// `sem_getvalue` (absent on macOS) to ask how many posts are outstanding.
    /// Taking one back before posting leaves the count at "one pending" whether
    /// or not a kick was already queued, so a burst of requests between two
    /// waits is one wakeup rather than a backlog that grows until the semaphore
    /// saturates. The sequence counters in the shared region remain the source
    /// of truth for *what* changed.
    pub fn set(&self) {
        unsafe {
            let _ = libc::sem_trywait(self.semaphore);
            let _ = libc::sem_post(self.semaphore);
        }
    }

    /// Take a pending kick if there is one, draining anything queued behind it.
    /// Draining is what gives this the auto-reset semantics of the Windows
    /// event: one wakeup consumes every request made since the last wait.
    fn try_acquire(&self) -> bool {
        if unsafe { libc::sem_trywait(self.semaphore) } != 0 {
            return false;
        }
        let mut drained = 0;
        while drained < KICK_MAX_DRAIN && unsafe { libc::sem_trywait(self.semaphore) } == 0 {
            drained += 1;
        }
        true
    }

    /// Block until signaled or `timeout_ms` elapses.
    ///
    /// Polls rather than blocking on a deadline because macOS has no
    /// `sem_timedwait` — see [`KICK_POLL_INTERVAL`]. The final sleep is clamped
    /// to the remaining time so the call never overruns its timeout.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        if self.try_acquire() {
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(u64::from(timeout_ms));
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                // One last look, so a kick that landed during the final sleep is
                // not deferred to the next call.
                return self.try_acquire();
            }
            std::thread::sleep(KICK_POLL_INTERVAL.min(deadline - now));
            if self.try_acquire() {
                return true;
            }
        }
    }
}

#[cfg(unix)]
impl Drop for BridgeKickEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::sem_close(self.semaphore);
            if self.owner {
                let _ = libc::sem_unlink(self.name.as_ptr());
            }
        }
    }
}

/// Unsupported-platform fallback: timeout-paced polling.
#[cfg(not(any(windows, unix)))]
pub struct BridgeKickEvent;

#[cfg(not(any(windows, unix)))]
impl BridgeKickEvent {
    pub fn create_named(_name: &str) -> std::io::Result<Self> {
        Ok(Self)
    }

    pub fn set(&self) {}

    pub fn wait(&self, timeout_ms: u32) -> bool {
        std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
        false
    }
}

impl std::fmt::Debug for BridgeKickEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeKickEvent").finish()
    }
}

#[cfg(windows)]
mod imp {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
        MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    };

    /// A pagefile-backed named file mapping + its mapped view. Drop unmaps the
    /// view and closes the section handle.
    pub struct WinMapping {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    // The mapped memory is shared; access is governed by the bridge's SPSC
    // contract, not by this handle.
    unsafe impl Send for WinMapping {}
    unsafe impl Sync for WinMapping {}

    impl WinMapping {
        pub fn create(name: &str, size: usize) -> std::io::Result<Self> {
            let wide = HSTRING::from(name);
            // SAFETY: INVALID_HANDLE_VALUE backs the mapping with the pagefile.
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    (size >> 32) as u32,
                    size as u32,
                    &wide,
                )
            }
            .map_err(std::io::Error::other)?;
            // Reject name squatting: `CreateFileMappingW` opens the existing
            // section (returning a valid handle) when the name is already taken,
            // signalling it only via `ERROR_ALREADY_EXISTS`. The creator side
            // must own a fresh, exclusively-created region — never map one another
            // process planted first.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "shared audio bridge name already in use (possible squatting)",
                ));
            }
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size) };
            if view.Value.is_null() {
                let err = std::io::Error::last_os_error();
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return Err(err);
            }
            Ok(Self { handle, view })
        }

        pub fn open(name: &str, size: usize) -> std::io::Result<Self> {
            let wide = HSTRING::from(name);
            let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &wide) }
                .map_err(std::io::Error::other)?;
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size) };
            if view.Value.is_null() {
                let err = std::io::Error::last_os_error();
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return Err(err);
            }
            Ok(Self { handle, view })
        }

        pub fn ptr(&self) -> *mut core::ffi::c_void {
            self.view.Value
        }
    }

    impl Drop for WinMapping {
        fn drop(&mut self) {
            unsafe {
                let _ = UnmapViewOfFile(self.view);
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;

    /// Longest POSIX IPC name that works on every platform we ship.
    ///
    /// macOS caps `shm_open`/`sem_open` names at 31 bytes including the leading
    /// slash (`PSHMNAMLEN`) and fails the call with `ENAMETOOLONG` past it.
    /// Linux allows `NAME_MAX`, so macOS is the binding constraint and the only
    /// one worth encoding here.
    pub(super) const POSIX_NAME_MAX: usize = 31;

    /// Fixed part of a generated name: `/fb_` + `_` + 16 hex digits of hash.
    const POSIX_NAME_OVERHEAD: usize = "/fb__".len() + 16;

    /// Convert the platform-neutral bridge name to a short POSIX IPC name.
    ///
    /// FNV-1a is fixed across processes, so both sides of the bridge derive the
    /// same name from the same source, and it keeps `/` and any platform-illegal
    /// bytes out of the result. The prefix is kept terse and the namespace is
    /// truncated if it ever grows, because the whole thing has to fit in
    /// [`POSIX_NAME_MAX`] — a longer name does not degrade, it fails the call.
    pub(super) fn posix_name(namespace: &str, source: &str) -> std::io::Result<CString> {
        let mut hash = 0xcbf29ce484222325u64;
        for byte in source.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let budget = POSIX_NAME_MAX - POSIX_NAME_OVERHEAD;
        let namespace: String = namespace.chars().take(budget).collect();
        CString::new(format!("/fb_{namespace}_{hash:016x}"))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid IPC name"))
    }

    /// Close-on-exec bit for `shm_open`, where the platform accepts one.
    ///
    /// macOS validates `shm_open`'s `oflag` against a small allowed set and
    /// rejects `O_CLOEXEC` with `EINVAL` — it is not implemented there. Every
    /// other Unix takes it and closes the window in which a concurrent
    /// `fork`+`exec` could leak the descriptor, so it is kept where it works and
    /// replaced by an explicit `FD_CLOEXEC` where it is not.
    #[cfg(target_vendor = "apple")]
    const SHM_CLOEXEC: libc::c_int = 0;
    #[cfg(not(target_vendor = "apple"))]
    const SHM_CLOEXEC: libc::c_int = libc::O_CLOEXEC;

    /// Marks `fd` close-on-exec when the open could not request it. The Studio
    /// spawns its plugin host, and neither should inherit the other's bridge
    /// descriptors. Best-effort: a leaked descriptor is not worth failing the
    /// bridge over.
    #[cfg(target_vendor = "apple")]
    fn set_cloexec(fd: libc::c_int) {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }

    /// No-op where `O_CLOEXEC` already did the job at open time.
    #[cfg(not(target_vendor = "apple"))]
    fn set_cloexec(_fd: libc::c_int) {}

    /// Names the syscall that produced the current `errno`.
    ///
    /// These calls differ per platform in ways that surface as a bare "Invalid
    /// argument", which says nothing about which of `shm_open`, `ftruncate` or
    /// `mmap` rejected what. The `ErrorKind` is preserved so callers matching on
    /// it (`AlreadyExists` for a squatted name) still work.
    fn last_error(call: &str) -> std::io::Error {
        let error = std::io::Error::last_os_error();
        std::io::Error::new(error.kind(), format!("{call}: {error}"))
    }

    pub struct UnixMapping {
        ptr: *mut core::ffi::c_void,
        size: usize,
        name: CString,
        owner: bool,
    }

    unsafe impl Send for UnixMapping {}
    unsafe impl Sync for UnixMapping {}

    impl UnixMapping {
        pub fn create(name: &str, size: usize) -> std::io::Result<Self> {
            let name = posix_name("audio", name)?;
            let fd = unsafe {
                libc::shm_open(
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | SHM_CLOEXEC,
                    super::IPC_OWNER_RW,
                )
            };
            if fd < 0 {
                return Err(last_error("shm_open (create)"));
            }
            set_cloexec(fd);
            if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
                let error = last_error("ftruncate");
                unsafe {
                    libc::close(fd);
                    libc::shm_unlink(name.as_ptr());
                }
                return Err(error);
            }
            Self::map_fd(fd, size, name, true)
        }

        pub fn open(name: &str, size: usize) -> std::io::Result<Self> {
            let name = posix_name("audio", name)?;
            let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | SHM_CLOEXEC, 0) };
            if fd < 0 {
                return Err(last_error("shm_open (open)"));
            }
            set_cloexec(fd);
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut stat) } != 0 {
                let error = last_error("fstat");
                unsafe {
                    libc::close(fd);
                }
                return Err(error);
            }
            if stat.st_size < size as libc::off_t {
                unsafe {
                    libc::close(fd);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "shared audio bridge is smaller than expected",
                ));
            }
            Self::map_fd(fd, size, name, false)
        }

        fn map_fd(
            fd: libc::c_int,
            size: usize,
            name: CString,
            owner: bool,
        ) -> std::io::Result<Self> {
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            let map_error = last_error("mmap");
            unsafe {
                libc::close(fd);
            }
            if ptr == libc::MAP_FAILED {
                if owner {
                    unsafe {
                        libc::shm_unlink(name.as_ptr());
                    }
                }
                return Err(map_error);
            }
            Ok(Self {
                ptr,
                size,
                name,
                owner,
            })
        }

        pub fn ptr(&self) -> *mut core::ffi::c_void {
            self.ptr
        }
    }

    impl Drop for UnixMapping {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr, self.size);
                if self.owner {
                    libc::shm_unlink(self.name.as_ptr());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips_in_process() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        assert!(!bridge.header_valid(), "blank region must not validate");
        bridge.init_header(48_000, 256, 2);
        assert!(bridge.header_valid());
        assert_eq!(bridge.sample_rate.load(Ordering::Relaxed), 48_000);
        assert_eq!(bridge.max_block_size.load(Ordering::Relaxed), 256);
        assert_eq!(bridge.plugin_output_channels(), 2);
        bridge.set_plugin_output_channels(8);
        assert_eq!(bridge.plugin_output_channels(), 8);
        assert!(!bridge.dsp_output_ready());
        bridge.set_dsp_output_ready(true);
        assert!(bridge.dsp_output_ready());
    }

    #[test]
    fn process_timing_counts_real_deadline_misses() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        bridge.init_header(48_000, 256, 2);

        // 64 / 48 kHz = 1,333.33 µs, rounded up to the first whole µs.
        bridge.record_process_timing(64, 1_333);
        assert_eq!(bridge.producer_deadline_misses.load(Ordering::Relaxed), 0);
        bridge.record_process_timing(64, 1_334);
        assert_eq!(bridge.producer_deadline_misses.load(Ordering::Relaxed), 1);
        assert_eq!(bridge.last_process_micros.load(Ordering::Relaxed), 1_334);
        assert_eq!(bridge.max_process_micros.load(Ordering::Relaxed), 1_334);
    }

    #[test]
    fn midi_ring_is_spsc_fifo_and_bounded() {
        let region = SharedAudioRegion::new_in_process();
        let ring = &region.bridge().midi;
        assert!(ring.is_empty());
        for i in 0..8u32 {
            assert!(ring.try_push(SharedMidiEvent {
                sample_offset: i,
                status: 0x90,
                data1: 60,
                data2: 100,
                _pad: 0,
            }));
        }
        assert_eq!(ring.len(), 8);
        let first = ring.try_pop().unwrap();
        assert_eq!(first.sample_offset, 0);
        // Fill to capacity, then the next push is dropped (wait-free, no block).
        let mut pushed = 1; // one popped above leaves room for one more than CAP-8
        while ring.try_push(SharedMidiEvent::default()) {
            pushed += 1;
        }
        assert!(pushed <= MIDI_RING_CAP, "ring must be bounded");
        ring.clear();
        assert!(ring.is_empty());
    }

    #[test]
    fn audio_buffer_copies_within_bounds() {
        let region = SharedAudioRegion::new_in_process();
        let buf = &region.bridge().audio_in;
        let src: Vec<f32> = (0..AUDIO_BUF_LEN).map(|i| i as f32).collect();
        unsafe { buf.write_interleaved(&src) };
        let mut dst = vec![0.0f32; AUDIO_BUF_LEN];
        unsafe { buf.read_interleaved(&mut dst, AUDIO_BUF_LEN) };
        assert_eq!(src, dst);
    }

    #[test]
    fn audio_buffer_short_write_zeros_stale_tail() {
        let region = SharedAudioRegion::new_in_process();
        let buf = &region.bridge().audio_out;
        let poison: Vec<f32> = (0..AUDIO_BUF_LEN).map(|i| (i + 1) as f32).collect();
        unsafe { buf.write_interleaved(&poison) };
        let short = [1.0f32, 2.0, 3.0, 4.0];
        unsafe { buf.write_interleaved(&short) };
        let mut dst = vec![0.0f32; AUDIO_BUF_LEN];
        unsafe { buf.read_interleaved(&mut dst, AUDIO_BUF_LEN) };
        assert_eq!(&dst[..4], &short);
        assert!(
            dst[4..].iter().all(|&s| s == 0.0),
            "tail after a short write must be cleared"
        );
    }

    #[test]
    fn audio_buffer_downmixes_multichannel_to_stereo() {
        let region = SharedAudioRegion::new_in_process();
        let buf = &region.bridge().audio_out;
        let frames = 2usize;
        let channels = 4usize;
        let src = [
            1.0f32, 2.0, 0.5, 0.25, // frame 0
            3.0, 4.0, 1.0, 2.0, // frame 1
        ];
        unsafe { buf.write_interleaved(&src) };

        let mut out_l = [0.0f32; 2];
        let mut out_r = [0.0f32; 2];
        let got = unsafe { buf.read_downmixed_to_stereo(&mut out_l, &mut out_r, frames, channels) };

        assert_eq!(got, frames);
        assert!((out_l[0] - (1.0 + 0.5 * 0.70710677)).abs() < 1e-6);
        assert!((out_r[0] - (2.0 + 0.25 * 0.70710677)).abs() < 1e-6);
        assert!((out_l[1] - (3.0 + 1.0 * 0.70710677)).abs() < 1e-6);
        assert!((out_r[1] - (4.0 + 2.0 * 0.70710677)).abs() < 1e-6);
    }

    #[test]
    fn audio_buffer_downmixes_only_selected_output_channels() {
        let region = SharedAudioRegion::new_in_process();
        let buf = &region.bridge().audio_out;
        let frames = 2usize;
        let channels = 4usize;
        let src = [
            1.0f32, 2.0, 10.0, 20.0, // frame 0
            3.0, 4.0, 30.0, 40.0, // frame 1
        ];
        unsafe { buf.write_interleaved(&src) };

        let mut out_l = [0.0f32; 2];
        let mut out_r = [0.0f32; 2];
        let got = unsafe {
            buf.read_downmixed_to_stereo_selected(&mut out_l, &mut out_r, frames, channels, &[3, 4])
        };

        assert_eq!(got, frames);
        assert!((out_l[0] - 10.0 * 0.70710677).abs() < 1e-6);
        assert!((out_r[0] - 20.0 * 0.70710677).abs() < 1e-6);
        assert!((out_l[1] - 30.0 * 0.70710677).abs() < 1e-6);
        assert!((out_r[1] - 40.0 * 0.70710677).abs() < 1e-6);
    }

    #[test]
    fn audio_buffer_empty_selection_downmixes_all_output_channels() {
        let region = SharedAudioRegion::new_in_process();
        let buf = &region.bridge().audio_out;
        let frames = 1usize;
        let channels = 4usize;
        let src = [1.0f32, 2.0, 10.0, 20.0];
        unsafe { buf.write_interleaved(&src) };

        let mut out_l = [0.0f32; 1];
        let mut out_r = [0.0f32; 1];
        let got = unsafe {
            buf.read_downmixed_to_stereo_selected(&mut out_l, &mut out_r, frames, channels, &[])
        };

        assert_eq!(got, frames);
        assert!((out_l[0] - (1.0 + 10.0 * 0.70710677)).abs() < 1e-6);
        assert!((out_r[0] - (2.0 + 20.0 * 0.70710677)).abs() < 1e-6);
    }

    #[test]
    fn audio_buffer_empty_selection_keeps_ad2_tail_bus_audible() {
        let region = SharedAudioRegion::new_in_process();
        let buf = &region.bridge().audio_out;
        let frames = 1usize;
        let channels = 28usize;
        let mut src = vec![0.0f32; frames * channels];
        src[26] = 0.75;
        src[27] = 0.5;
        unsafe { buf.write_interleaved(&src) };

        let mut out_l = [0.0f32; 1];
        let mut out_r = [0.0f32; 1];
        let got = unsafe {
            buf.read_downmixed_to_stereo_selected(&mut out_l, &mut out_r, frames, channels, &[])
        };

        assert_eq!(got, frames);
        assert!((out_l[0] - 0.75 * 0.70710677).abs() < 1e-6);
        assert!((out_r[0] - 0.5 * 0.70710677).abs() < 1e-6);
    }

    #[test]
    fn meters_round_trip_through_bits() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        bridge.store_meters(0.5, 0.25);
        assert_eq!(bridge.meters(), (0.5, 0.25));
    }

    /// A DSP that publishes no telemetry must be distinguishable from one that
    /// measured silence — otherwise the editor reads the zeroed region as a
    /// real, and wrong, meter reading.
    #[test]
    fn builtin_meters_are_absent_until_published_even_when_silent() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        assert!(bridge.builtin_meters().is_none());

        let silent = BuiltinMeterFrame::default();
        bridge.store_builtin_meters(&silent);
        assert_eq!(bridge.builtin_meters(), Some(silent));

        let loud = BuiltinMeterFrame {
            in_peak: 0.5,
            in_rms: 0.25,
            out_peak: 0.75,
            out_rms: 0.5,
            gain_reduction_db: 6.5,
            in_clip: false,
            out_clip: true,
            slot_in_peak: [0.9, 0.8, 0.0, 0.0, 0.0, 0.0],
            slot_out_peak: [0.7, 0.6, 0.0, 0.0, 0.0, 0.0],
        };
        bridge.store_builtin_meters(&loud);
        assert_eq!(bridge.builtin_meters(), Some(loud));
    }

    #[test]
    fn per_stage_telemetry_round_trips_and_defaults_to_silent() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        // A built-in with no rack publishes zeros, and those must survive the
        // round trip as zeros rather than reading as uninitialised bits.
        let flat = BuiltinMeterFrame {
            in_peak: 0.4,
            ..BuiltinMeterFrame::default()
        };
        bridge.store_builtin_meters(&flat);
        let read = bridge.builtin_meters().expect("published");
        assert_eq!(read.slot_in_peak, [0.0; BUILTIN_RACK_SLOTS]);
        assert_eq!(read.slot_out_peak, [0.0; BUILTIN_RACK_SLOTS]);

        let racked = BuiltinMeterFrame {
            slot_in_peak: [0.1, 0.2, 0.3, 0.4, 0.5, 0.6],
            slot_out_peak: [0.6, 0.5, 0.4, 0.3, 0.2, 0.1],
            ..BuiltinMeterFrame::default()
        };
        bridge.store_builtin_meters(&racked);
        assert_eq!(bridge.builtin_meters(), Some(racked));
    }

    /// An insert that measures no image never publishes one, and a published
    /// frame comes back field for field under a rising sequence.
    #[test]
    fn stereo_image_is_absent_until_published_then_round_trips() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        assert!(bridge.stereo_image().is_none());

        let mut frame = StereoImageFrame {
            correlation: -0.25,
            band_correlation: [1.0, 0.5, 0.0, -1.0],
            band_level: [0.1, 0.2, 0.3, 0.4],
            ..StereoImageFrame::default()
        };
        frame.scope[0] = 0.75;
        frame.scope[IMAGE_SCOPE_POINTS * 2 - 1] = -0.5;
        bridge.store_stereo_image(&frame);
        let (first, read) = bridge.stereo_image().expect("published");
        assert_eq!(read, frame);

        bridge.store_stereo_image(&StereoImageFrame::default());
        let (second, read) = bridge.stereo_image().expect("published");
        assert!(second > first);
        assert_eq!(read, StereoImageFrame::default());
    }

    #[test]
    fn band_reduction_is_absent_until_published_then_round_trips() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        assert!(bridge.band_reduction().is_none());
        bridge.store_band_reduction(&[6.5, 0.0, 1.25, 0.0]);
        let (first, read) = bridge.band_reduction().expect("published");
        assert_eq!(read, [6.5, 0.0, 1.25, 0.0]);
        bridge.store_band_reduction(&[0.0; REDUCTION_BANDS]);
        let (second, read) = bridge.band_reduction().expect("published");
        assert!(second > first);
        assert_eq!(read, [0.0; REDUCTION_BANDS]);
    }

    #[test]
    fn pad_levels_are_absent_until_published_then_round_trip() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        assert!(bridge.pad_levels().is_none());
        let mut levels = [0.0f32; BUILTIN_PAD_SLOTS];
        levels[0] = 0.5;
        levels[BUILTIN_PAD_SLOTS - 1] = 0.125;
        bridge.store_pad_levels(&levels);
        let (first, read) = bridge.pad_levels().expect("published");
        assert_eq!(read, levels);
        bridge.store_pad_levels(&[0.0; BUILTIN_PAD_SLOTS]);
        let (second, read) = bridge.pad_levels().expect("published");
        assert!(second > first);
        assert_eq!(read, [0.0; BUILTIN_PAD_SLOTS]);
    }

    #[test]
    fn transport_defaults_then_round_trips() {
        let region = SharedAudioRegion::new_in_process();
        let bridge = region.bridge();
        bridge.init_header(48_000, 256, 2);
        // Sane defaults before the first publish.
        let def = bridge.load_transport();
        assert_eq!(def.tempo_bpm, 120.0);
        assert_eq!((def.time_sig_num, def.time_sig_den), (4, 4));
        assert!(!def.playing && !def.recording);

        let pushed = BridgeTransport {
            tempo_bpm: 140.5,
            time_sig_num: 3,
            time_sig_den: 8,
            project_time_samples: 96_000,
            ppq_position: 12.25,
            bar_position_ppq: 12.0,
            playing: true,
            recording: false,
        };
        bridge.store_transport(&pushed);
        assert_eq!(bridge.load_transport(), pushed);
    }

    /// Every generated POSIX IPC name has to fit macOS's 31-byte cap, including
    /// the leading slash. Past it `shm_open`/`sem_open` fail outright with
    /// `ENAMETOOLONG` — there is no graceful truncation — so the whole bridge
    /// stops working on macOS while Linux and Windows stay green. Sources here
    /// are the real ones: a Windows-style section name and a kick-event name.
    #[cfg(unix)]
    #[test]
    fn posix_ipc_names_fit_the_platform_limit() {
        let sources = [
            format!("Local\\FutureboardBridge-{}-insert-track-1-1", u32::MAX),
            bridge_kick_event_name(u32::MAX, u32::MAX),
            String::new(),
        ];
        for namespace in ["audio", "kick"] {
            for source in &sources {
                let name = imp::posix_name(namespace, source).expect("name builds");
                let len = name.as_bytes().len();
                assert!(
                    len <= imp::POSIX_NAME_MAX,
                    "{namespace}/{source} produced {len} bytes: {name:?}"
                );
                assert!(
                    name.to_str().expect("ascii").starts_with('/'),
                    "a POSIX IPC name must be rooted: {name:?}"
                );
            }
        }
    }

    /// The same source must always resolve to the same name — both sides of the
    /// bridge derive it independently — and different sources must not collide.
    #[cfg(unix)]
    #[test]
    fn posix_ipc_names_are_stable_and_distinct() {
        let a = imp::posix_name("audio", "bridge-one").expect("name builds");
        let b = imp::posix_name("audio", "bridge-one").expect("name builds");
        let c = imp::posix_name("audio", "bridge-two").expect("name builds");
        let d = imp::posix_name("kick", "bridge-one").expect("name builds");
        assert_eq!(a, b);
        assert_ne!(a, c, "different bridges must not share a region");
        assert_ne!(a, d, "the kick event must not collide with the region");
    }

    /// A second creator on an already-taken section name must be rejected rather
    /// than silently handed the existing region (name-squatting defence).
    #[cfg(any(windows, unix))]
    #[test]
    fn create_named_rejects_squatted_name() {
        let name = format!("Local\\FutureboardBridgeSquatTest-{}", std::process::id());
        let first =
            SharedAudioRegion::create_named(&name, 48_000, 256, 2).expect("first create succeeds");
        let second = SharedAudioRegion::create_named(&name, 48_000, 256, 2);
        assert!(second.is_err(), "squatted name must be rejected");
        drop(first);
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn named_region_maps_the_same_audio_bridge() {
        let name = format!("Local\\FutureboardBridgeMapTest-{}", std::process::id());
        let engine =
            SharedAudioRegion::create_named(&name, 48_000, 256, 2).expect("create shared region");
        let host = SharedAudioRegion::open_named(&name).expect("open shared region");

        assert_eq!(host.bridge().sample_rate.load(Ordering::Relaxed), 48_000);
        engine.bridge().request_seq.store(42, Ordering::Release);
        assert_eq!(host.bridge().request_seq.load(Ordering::Acquire), 42);
        host.bridge().done_seq.store(42, Ordering::Release);
        assert_eq!(engine.bridge().done_seq.load(Ordering::Acquire), 42);
    }

    /// The producer-wake event pairs a creator and an opener on the same name:
    /// `set` on one handle wakes a `wait` on the other, and the auto-reset
    /// consumes each signal exactly once.
    #[cfg(any(windows, unix))]
    #[test]
    fn kick_event_wakes_waiter_and_auto_resets() {
        // Unique per test process; 0xFFFF_FFFF is never a real host pid.
        let name = bridge_kick_event_name(std::process::id(), 0xFFFF_FFFF);
        let engine_side = BridgeKickEvent::create_named(&name).expect("create kick event");
        let host_side = BridgeKickEvent::create_named(&name).expect("open kick event");

        assert!(!host_side.wait(0), "must start unsignaled");
        engine_side.set();
        assert!(host_side.wait(100), "set must wake the waiter");
        assert!(!host_side.wait(0), "auto-reset must consume the signal");
    }
}

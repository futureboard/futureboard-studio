//! Solfege Audioguard: dropout protection for the realtime output.
//!
//! Three parts, all off the hot path's critical sections:
//!
//! * **Render-ahead.** While nothing is live — no take recording, no input
//!   monitored or armed, no Audio Jam, no notes played in the last moments —
//!   a guard thread renders the graph a few blocks ahead of the device into a
//!   preallocated ring, and the device callback only copies the next block out.
//!   A plug-in that takes longer than one period on some blocks is then absorbed
//!   by the ring instead of being heard as a dropout. As soon as anything turns
//!   live the guard stops producing, the ring plays out, and the callback goes
//!   back to rendering every block itself, so a monitored input or a played note
//!   never waits behind the ring.
//! * **Denormal flushing.** Every thread that renders runs with flush-to-zero
//!   and denormals-are-zero set, so a reverb or filter tail decaying into the
//!   denormal range does not cost a hundred times its normal CPU.
//! * **Output guard.** The device output is muted for a moment when it carries
//!   invalid samples (NaN / infinity) or a burst far beyond full scale — a
//!   feedback loop or a broken plug-in — instead of sending it to the speakers.
//!
//! Ownership: [`RenderCore`] holds everything the render owns (the graph, the
//! per-stream state and the command queue). It sits behind a mutex the device
//! callback only ever `try_lock`s, so the callback never blocks on it. Whoever
//! holds it renders the next block at the graph's cursor; the ring holds blocks
//! in order, so falling back from the ring to rendering inline is seamless.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use parking_lot::Mutex;

use crate::backend::render::{drain_commands, fill_output_f32, LocalAudioState};
use crate::command::EngineCommand;
use crate::engine::{AudioEngineState, DropoutReason, SharedState};
use crate::runtime::RuntimeProject;

// ── Level ────────────────────────────────────────────────────────────────────

/// Audioguard level (Preferences → Playback). Stored in
/// [`SharedState::dropout_protection_mode`] as its `u8`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioguardLevel {
    /// No render-ahead and no output guard: every block is rendered inside the
    /// device callback, as it is played.
    Off = 0,
    /// Render-ahead of a couple of blocks, invalid-output and burst guard,
    /// automatic bypass of a plug-in that keeps producing invalid audio.
    Normal = 2,
    /// A deeper ring, and a bridged plug-in that keeps missing its deadline is
    /// bypassed too.
    High = 3,
}

impl AudioguardLevel {
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Off,
            3 => Self::High,
            // 1 was the old "Light" mode; it and 2 ("Medium") are Normal now.
            _ => Self::Normal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Normal => "Normal",
            Self::High => "High",
        }
    }

    /// Fraction `(num, den)` of the per-block deadline at or above which a
    /// block rendered *in the device callback* is counted as a dropout risk.
    #[inline]
    pub fn dropout_threshold_ratio(self) -> (u64, u64) {
        match self {
            Self::Off => (100, 100),
            Self::Normal => (80, 100),
            Self::High => (70, 100),
        }
    }

    /// Whether the output guard and the plug-in checks run.
    #[inline]
    pub fn protects(self) -> bool {
        self != Self::Off
    }

    /// How far ahead of the device the guard keeps the ring, in frames, for a
    /// device block of `block_frames` at `sample_rate`. `0` when off.
    pub fn render_ahead_frames(self, block_frames: usize, sample_rate: u32) -> usize {
        let (blocks, millis) = match self {
            Self::Off => return 0,
            Self::Normal => (2usize, 12u64),
            Self::High => (4usize, 40u64),
        };
        let by_time = (sample_rate as u64 * millis / 1000) as usize;
        let wanted = (block_frames * blocks).max(by_time);
        // Whole blocks, and never more than the ring holds (one slot is kept
        // free so the producer never waits on the consumer's partial slot).
        let wanted_blocks = wanted.div_ceil(block_frames.max(1));
        wanted_blocks.min(GUARD_SLOTS - 2) * block_frames
    }
}

/// Process-wide copy of the level for code that has no [`SharedState`] at
/// hand (the per-insert checks inside the track chain). Written by
/// [`set_process_level`], one relaxed load per use.
static PROCESS_LEVEL: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(AudioguardLevel::Normal as u8);

pub(crate) fn set_process_level(level: AudioguardLevel) {
    PROCESS_LEVEL.store(level as u8, Ordering::Relaxed);
}

#[inline]
pub(crate) fn process_level() -> AudioguardLevel {
    AudioguardLevel::from_u8(PROCESS_LEVEL.load(Ordering::Relaxed))
}

// ── Denormals ────────────────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
const MXCSR_FTZ_DAZ: u32 = (1 << 15) | (1 << 6);
#[cfg(target_arch = "aarch64")]
const FPCR_FZ: u64 = 1 << 24;

#[cfg(target_arch = "x86_64")]
#[inline]
fn read_fp_control() -> u64 {
    let mut csr: u32 = 0;
    // SAFETY: `stmxcsr` stores the 32-bit MXCSR into the given location.
    unsafe {
        core::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags));
    }
    csr as u64
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn write_fp_control(value: u64) {
    let csr = value as u32;
    // SAFETY: `ldmxcsr` loads MXCSR from the given location; the value only
    // differs from the current one in the FTZ / DAZ bits.
    unsafe {
        core::arch::asm!("ldmxcsr [{}]", in(reg) &csr, options(nostack, readonly, preserves_flags));
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn read_fp_control() -> u64 {
    let value: u64;
    // SAFETY: reading FPCR has no side effects.
    unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) value, options(nomem, nostack)) };
    value
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn write_fp_control(value: u64) {
    // SAFETY: only the FZ bit differs from the current FPCR.
    unsafe { core::arch::asm!("msr fpcr, {}", in(reg) value, options(nomem, nostack)) };
}

#[inline]
fn flushing_control(value: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        value | MXCSR_FTZ_DAZ as u64
    }
    #[cfg(target_arch = "aarch64")]
    {
        value | FPCR_FZ
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        value
    }
}

/// Flush-to-zero and denormals-are-zero for the scope of one render, then the
/// thread's own setting back. The device callback runs on a thread the driver
/// owns, so it is not left changed. Two instructions each way.
pub(crate) struct DenormalScope {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    saved: u64,
}

impl DenormalScope {
    #[inline]
    pub(crate) fn enter() -> Self {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            let saved = read_fp_control();
            let flushing = flushing_control(saved);
            if flushing != saved {
                write_fp_control(flushing);
            }
            Self { saved }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Self {}
        }
    }
}

impl Drop for DenormalScope {
    #[inline]
    fn drop(&mut self) {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        if read_fp_control() != self.saved {
            write_fp_control(self.saved);
        }
    }
}

/// Flush denormals on the current thread for good. For threads the engine
/// owns and only ever renders on (the parallel workers, the guard thread).
pub(crate) fn flush_denormals_on_this_thread() {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    write_fp_control(flushing_control(read_fp_control()));
}

// ── Output guard ─────────────────────────────────────────────────────────────

/// Peak above which the device output is treated as a burst: +12 dBFS. Far
/// past anything a mix plays on purpose; what a feedback loop or a plug-in
/// gone wrong produces.
pub(crate) const BURST_LIMIT: f32 = 3.981_072;
/// How long the output stays muted after a burst or invalid samples.
const GUARD_MUTE_SECONDS: f64 = 0.5;
/// The fade back in after the mute.
const GUARD_FADE_IN_SECONDS: f64 = 0.05;

/// What the output guard last stopped, for the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputGuardReason {
    None = 0,
    InvalidSamples = 1,
    Burst = 2,
}

impl OutputGuardReason {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::InvalidSamples,
            2 => Self::Burst,
            _ => Self::None,
        }
    }
}

/// Per-stream output guard state. Lives in [`LocalAudioState`].
#[derive(Debug, Default)]
pub struct OutputGuard {
    /// Frames left to keep the output muted.
    mute_remaining: u64,
    /// Frames into the fade back in, and its length.
    fade_pos: u64,
    fade_len: u64,
}

impl OutputGuard {
    /// Check and, when needed, mute one interleaved device block. Realtime-safe:
    /// one scan, and a fill only when it trips.
    pub(crate) fn process(
        &mut self,
        data: &mut [f32],
        channels: usize,
        sample_rate: u32,
        shared: &SharedState,
    ) {
        let channels = channels.max(1);
        let frames = (data.len() / channels) as u64;
        if frames == 0 {
            return;
        }
        let mut reason = OutputGuardReason::None;
        for &sample in data.iter() {
            if !sample.is_finite() {
                reason = OutputGuardReason::InvalidSamples;
                break;
            }
            if sample.abs() > BURST_LIMIT {
                reason = OutputGuardReason::Burst;
            }
        }
        if reason != OutputGuardReason::None {
            let rate = sample_rate.max(1) as f64;
            self.mute_remaining = (GUARD_MUTE_SECONDS * rate) as u64;
            self.fade_len = (GUARD_FADE_IN_SECONDS * rate).max(1.0) as u64;
            self.fade_pos = 0;
            shared.guard_output_mutes.fetch_add(1, Ordering::Relaxed);
            shared
                .guard_output_reason
                .store(reason as u8, Ordering::Relaxed);
            data.fill(0.0);
            return;
        }
        if self.mute_remaining > 0 {
            let muted = self.mute_remaining.min(frames) as usize;
            data[..muted * channels].fill(0.0);
            self.mute_remaining -= muted as u64;
            if self.mute_remaining > 0 {
                return;
            }
            self.apply_fade(&mut data[muted * channels..], channels);
            return;
        }
        if self.fade_pos < self.fade_len {
            self.apply_fade(data, channels);
        }
    }

    fn apply_fade(&mut self, data: &mut [f32], channels: usize) {
        for frame in data.chunks_mut(channels) {
            if self.fade_pos >= self.fade_len {
                break;
            }
            let gain = self.fade_pos as f32 / self.fade_len as f32;
            for sample in frame.iter_mut() {
                *sample *= gain;
            }
            self.fade_pos += 1;
        }
    }
}

// ── Plug-in guard ────────────────────────────────────────────────────────────

/// Why Audioguard bypassed an insert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertTrip {
    None = 0,
    /// It produced NaN / infinity on several blocks in a row.
    InvalidOutput = 1,
    /// A bridged plug-in kept missing its block (High only).
    MissedDeadlines = 2,
}

impl InsertTrip {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::InvalidOutput,
            2 => Self::MissedDeadlines,
            _ => Self::None,
        }
    }
}

/// What the control side reads about one insert. Shared by every clone of
/// the insert (the callback's graph and the control thread's mirror).
#[derive(Debug, Default)]
pub struct InsertHealth {
    /// An [`InsertTrip`] as `u8`; set once by the render, never cleared there.
    pub tripped: std::sync::atomic::AtomicU8,
    /// The control side has reported the trip.
    pub reported: AtomicBool,
}

/// An insert Audioguard bypassed, reported once to the studio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrippedInsert {
    pub track_id: String,
    pub insert_id: String,
    pub reason: InsertTrip,
}

/// Audioguard's state for the control side. Atomic reads only.
#[derive(Clone, Copy, Debug)]
pub struct AudioguardStatus {
    pub level: AudioguardLevel,
    /// Rendering ahead right now, and by how much (milliseconds).
    pub rendering_ahead: bool,
    pub ahead_ms: f32,
    /// Device blocks the ring could not supply.
    pub ring_misses: u64,
    /// Blocks the device did not get in time, every cause together: a block
    /// rendered in the callback past its deadline, a ring miss, an underrun
    /// the device itself reported. What a System Overloaded warning counts.
    pub overruns: u64,
    /// Times the output guard muted the output, and why it last did.
    pub output_mutes: u64,
    pub last_output_reason: OutputGuardReason,
    /// The output callback thread runs at MMCSS "Pro Audio" (Windows).
    pub mmcss_active: bool,
}

impl AudioguardStatus {
    pub(crate) fn read(shared: &SharedState) -> Self {
        let rate = shared.sample_rate.load(Ordering::Relaxed).max(1) as f32;
        let engaged = shared.guard_engaged.load(Ordering::Relaxed);
        Self {
            level: AudioguardLevel::from_u8(shared.dropout_protection_mode.load(Ordering::Relaxed)),
            rendering_ahead: engaged,
            ahead_ms: if engaged {
                shared.guard_ahead_frames.load(Ordering::Relaxed) as f32 * 1000.0 / rate
            } else {
                0.0
            },
            ring_misses: shared.guard_misses.load(Ordering::Relaxed),
            overruns: shared.callback_overruns.load(Ordering::Relaxed)
                + shared.guard_misses.load(Ordering::Relaxed)
                + shared.device_xruns.load(Ordering::Relaxed),
            output_mutes: shared.guard_output_mutes.load(Ordering::Relaxed),
            last_output_reason: OutputGuardReason::from_u8(
                shared.guard_output_reason.load(Ordering::Relaxed),
            ),
            mmcss_active: shared.mmcss_active.load(Ordering::Relaxed),
        }
    }
}

/// Inserts in `runtime` that Audioguard bypassed and nobody has been told
/// about yet; marks them told. Control thread only (it reads the mirror).
pub(crate) fn take_tripped_inserts(runtime: &RuntimeProject) -> Vec<TrippedInsert> {
    let mut tripped = Vec::new();
    for track in &runtime.tracks {
        for insert in &track.inserts {
            let health = &insert.guard.health;
            let reason = InsertTrip::from_u8(health.tripped.load(Ordering::Relaxed));
            if reason == InsertTrip::None || health.reported.swap(true, Ordering::Relaxed) {
                continue;
            }
            tripped.push(TrippedInsert {
                track_id: track.id.clone(),
                insert_id: insert.id.clone(),
                reason,
            });
        }
    }
    tripped
}

/// Render-side guard state of one insert.
#[derive(Debug, Clone, Default)]
pub struct InsertGuard {
    pub health: Arc<InsertHealth>,
    /// Bypassed by Audioguard until the graph is rebuilt (which the studio
    /// does when it turns the insert off in the project).
    pub bypassed: bool,
    invalid_run: u8,
    window_blocks: u32,
    window_misses: u32,
}

/// Consecutive invalid blocks before an insert is bypassed.
const INVALID_RUN_TO_TRIP: u8 = 3;
/// Blocks per miss-rate window for bridged plug-ins, and the share of misses
/// (1 in `MISS_SHARE_TO_TRIP`) that trips it.
const MISS_WINDOW_BLOCKS: u32 = 512;
const MISS_SHARE_TO_TRIP: u32 = 4;

impl InsertGuard {
    /// Process again and watch afresh: the user switched the insert back on.
    /// Realtime-safe (stores only); the old health stays tripped for whoever
    /// still reads it, a new one would allocate.
    pub(crate) fn rearm(&mut self) {
        self.bypassed = false;
        self.invalid_run = 0;
        self.window_blocks = 0;
        self.window_misses = 0;
        self.health
            .tripped
            .store(InsertTrip::None as u8, Ordering::Relaxed);
        self.health.reported.store(false, Ordering::Relaxed);
    }

    fn trip(&mut self, why: InsertTrip) {
        self.bypassed = true;
        self.health.tripped.store(why as u8, Ordering::Relaxed);
    }

    /// Check what the insert just wrote. A non-finite block is silenced here,
    /// so the next insert never sees it; a run of them bypasses the insert.
    /// Realtime-safe: one scan, and a fill when it is bad.
    #[inline]
    pub(crate) fn check_output(&mut self, block_l: &mut [f32], block_r: &mut [f32]) {
        let finite = block_l.iter().all(|s| s.is_finite()) && block_r.iter().all(|s| s.is_finite());
        if !finite {
            block_l.fill(0.0);
            block_r.fill(0.0);
            self.invalid_run = self.invalid_run.saturating_add(1);
            if self.invalid_run >= INVALID_RUN_TO_TRIP {
                self.trip(InsertTrip::InvalidOutput);
            }
        } else {
            self.invalid_run = 0;
        }
    }

    /// Count one bridged block, `missed` when the host had nothing fresh.
    ///
    /// At High, a plug-in that misses one device-callback block in four over
    /// a window is bypassed. A block rendered ahead does not count: the guard
    /// asks again immediately, and the host still finishing the previous
    /// block is the latency the ring exists to hide. Counting those misses
    /// bypasses every bridged effect as soon as High is rendering ahead.
    #[inline]
    pub(crate) fn note_bridge_block(&mut self, missed: bool) {
        self.note_bridge_block_at(
            missed,
            process_level(),
            crate::plugin_bridge::render_ahead_grace().is_some(),
        );
    }

    fn note_bridge_block_at(&mut self, missed: bool, level: AudioguardLevel, rendered_ahead: bool) {
        if level != AudioguardLevel::High {
            self.window_blocks = 0;
            self.window_misses = 0;
            return;
        }
        if rendered_ahead {
            return;
        }
        self.window_blocks += 1;
        self.window_misses += missed as u32;
        if self.window_blocks >= MISS_WINDOW_BLOCKS {
            if self.window_misses * MISS_SHARE_TO_TRIP >= self.window_blocks {
                self.trip(InsertTrip::MissedDeadlines);
            }
            self.window_blocks = 0;
            self.window_misses = 0;
        }
    }
}

// ── Render core ──────────────────────────────────────────────────────────────

/// Everything one output stream renders with. Owned by whoever renders the
/// next block: the device callback, or the guard thread while it runs ahead.
pub struct RenderCore {
    pub runtime: RuntimeProject,
    pub local: LocalAudioState,
    cmd_rx: Receiver<EngineCommand>,
    sample_rate: u32,
}

impl RenderCore {
    pub fn new(
        runtime: RuntimeProject,
        local: LocalAudioState,
        cmd_rx: Receiver<EngineCommand>,
        sample_rate: u32,
    ) -> Self {
        Self {
            runtime,
            local,
            cmd_rx,
            sample_rate,
        }
    }

    /// Drain the command queue and render one block into `out`.
    #[inline]
    fn render(&mut self, out: &mut [f32], channels: usize, shared: &Arc<SharedState>) {
        let _denormals = DenormalScope::enter();
        drain_commands(
            &self.cmd_rx,
            &mut self.runtime,
            shared,
            &mut self.local,
            self.sample_rate,
        );
        fill_output_f32(out, channels, &mut self.runtime, shared, &mut self.local);
    }

    /// Whether anything in the graph has to be heard as it happens: a track
    /// armed, monitoring its input, or fed by or feeding an Audio Jam.
    fn has_live_tracks(&self) -> bool {
        self.runtime.tracks.iter().any(|track| {
            track.record_armed
                || track.monitor_enabled
                || track.jam_publish_slot.is_some()
                || track.jam_multitrack_pair.is_some()
                || matches!(
                    track.input_source,
                    crate::runtime::RuntimeTrackInputSource::Jam { .. }
                )
        })
    }
}

// ── Ring ─────────────────────────────────────────────────────────────────────

/// Blocks the ring holds.
pub(crate) const GUARD_SLOTS: usize = 32;
/// Largest device block the guard renders ahead. Larger buffers already have
/// the headroom render-ahead would add, and render inline.
pub(crate) const MAX_GUARD_BLOCK_FRAMES: usize = 2048;

struct Slot {
    samples: Vec<f32>,
    frames: usize,
    /// Transport sample the block starts at, and the cursor after it.
    start: u64,
    next_position: u64,
    playing: bool,
}

/// Single-producer (guard thread) / single-consumer (device callback) ring of
/// rendered blocks. Slots are allocated once; nothing is allocated after.
struct GuardRing {
    slots: Box<[UnsafeCell<Slot>]>,
    /// Blocks consumed / published, monotonically counted.
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: a slot is written only by the producer while `tail - head <
// GUARD_SLOTS` says the consumer is not reading it, and read only by the
// consumer between the producer's Release publish and its own Release pop.
unsafe impl Sync for GuardRing {}
unsafe impl Send for GuardRing {}

impl GuardRing {
    fn new(channels: usize) -> Self {
        let slots = (0..GUARD_SLOTS)
            .map(|_| {
                UnsafeCell::new(Slot {
                    samples: vec![0.0; MAX_GUARD_BLOCK_FRAMES * channels],
                    frames: 0,
                    start: 0,
                    next_position: 0,
                    playing: false,
                })
            })
            .collect();
        Self {
            slots,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.tail
            .load(Ordering::Acquire)
            .wrapping_sub(self.head.load(Ordering::Acquire))
    }
}

// ── Guarded renderer ─────────────────────────────────────────────────────────

struct GuardInner {
    core: Mutex<RenderCore>,
    ring: GuardRing,
    channels: usize,
    shared: Arc<SharedState>,
    /// Device block size the callback last asked for (frames).
    block_frames: AtomicUsize,
    stop: AtomicBool,
    /// Set when a render on the guard thread panicked: render-ahead stays off
    /// for this stream.
    failed: AtomicBool,
}

/// The device side of one output stream: renders each device block from the
/// ring when the guard has it ready, and inline otherwise.
pub struct GuardedRenderer {
    inner: Arc<GuardInner>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Frames of the ring's front slot already played.
    front_offset: usize,
}

impl GuardedRenderer {
    /// Wrap a stream's render state. `channels` is the device's interleaved
    /// channel count; it does not change for the life of the stream.
    pub fn new(core: RenderCore, channels: usize, shared: Arc<SharedState>) -> Self {
        let channels = channels.max(1);
        let inner = Arc::new(GuardInner {
            core: Mutex::new(core),
            ring: GuardRing::new(channels),
            channels,
            shared,
            block_frames: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        inner
            .shared
            .guard_pending_frames
            .store(0, Ordering::Relaxed);
        let thread_inner = Arc::clone(&inner);
        let thread = std::thread::Builder::new()
            .name("Audioguard".to_string())
            .spawn(move || guard_thread(thread_inner))
            .map_err(|error| eprintln!("[Audioguard] guard thread not started: {error}"))
            .ok();
        Self {
            inner,
            thread,
            front_offset: 0,
        }
    }

    /// Run `f` on the render state with the device callback's own lock
    /// discipline: blocks only until the guard finishes the block it is on.
    /// For the stream's start-up (WDM-KS prefill), never the callback.
    pub fn with_core<R>(&self, f: impl FnOnce(&mut RenderCore) -> R) -> R {
        f(&mut self.inner.core.lock())
    }

    /// Fill one interleaved device block. Realtime-safe: a copy from the ring,
    /// or an inline render under a `try_lock`; never a blocking wait.
    pub fn render(&mut self, out: &mut [f32]) {
        let inner = &*self.inner;
        let offset = &mut self.front_offset;
        let channels = inner.channels;
        let shared = &inner.shared;
        let need = out.len() / channels;
        inner.block_frames.store(need, Ordering::Relaxed);
        let mut filled = take_from_ring(inner, offset, out, 0, need);
        if filled < need {
            if let Some(mut core) = inner.core.try_lock() {
                // The ring is empty here, so the graph's cursor is right after
                // the last frame played: rendering on continues seamlessly.
                core.render(&mut out[filled * channels..], channels, shared);
            } else {
                // The guard is rendering the very block this needs. Give it
                // part of the period, then fall back to whatever is ready.
                let deadline = Instant::now() + block_period(need, shared).mul_f32(0.5);
                while filled < need && Instant::now() < deadline {
                    std::hint::spin_loop();
                    filled = take_from_ring(inner, offset, out, filled, need);
                    if filled < need {
                        if let Some(mut core) = inner.core.try_lock() {
                            filled = take_from_ring(inner, offset, out, filled, need);
                            if filled < need {
                                core.render(&mut out[filled * channels..], channels, shared);
                                filled = need;
                            }
                        }
                    }
                }
                if filled < need {
                    out[filled * channels..].fill(0.0);
                    shared.dropout_count.fetch_add(1, Ordering::Relaxed);
                    shared.guard_misses.fetch_add(1, Ordering::Relaxed);
                    shared
                        .dropout_last_reason
                        .store(DropoutReason::RenderAheadMiss as u8, Ordering::Relaxed);
                }
            }
        }
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }
}

/// Copy frames `from..need` of `out` from the ring. Returns how far `out` is
/// filled after it. Publishes the played block clock as it goes. Consumer side
/// only (the device callback).
fn take_from_ring(
    inner: &GuardInner,
    front_offset: &mut usize,
    out: &mut [f32],
    mut from: usize,
    need: usize,
) -> usize {
    let channels = inner.channels;
    let ring = &inner.ring;
    let shared = &inner.shared;
    while from < need {
        let head = ring.head.load(Ordering::Relaxed);
        if ring.tail.load(Ordering::Acquire) == head {
            break;
        }
        // SAFETY: `head < tail`, so the producer has published this slot and
        // will not write it again until `head` moves past it.
        let slot = unsafe { &*ring.slots[head % GUARD_SLOTS].get() };
        let take = (slot.frames - *front_offset).min(need - from);
        let src = &slot.samples[*front_offset * channels..(*front_offset + take) * channels];
        out[from * channels..(from + take) * channels].copy_from_slice(src);
        if slot.playing {
            shared.note_output_block(slot.start + *front_offset as u64, take as u64);
        }
        *front_offset += take;
        let cursor = if *front_offset == slot.frames {
            slot.next_position
        } else {
            slot.start + *front_offset as u64
        };
        shared.guard_played_cursor.store(cursor, Ordering::Relaxed);
        shared
            .guard_pending_frames
            .fetch_sub(take as u64, Ordering::AcqRel);
        from += take;
        if *front_offset == slot.frames {
            *front_offset = 0;
            ring.head.store(head.wrapping_add(1), Ordering::Release);
        }
    }
    from
}

impl Drop for GuardedRenderer {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
        self.inner
            .shared
            .guard_pending_frames
            .store(0, Ordering::Relaxed);
        self.inner
            .shared
            .guard_engaged
            .store(false, Ordering::Relaxed);
    }
}

#[inline]
fn block_period(frames: usize, shared: &SharedState) -> Duration {
    let rate = shared.sample_rate.load(Ordering::Relaxed).max(1) as u64;
    Duration::from_nanos(frames as u64 * 1_000_000_000 / rate)
}

/// How long after the last played note or controller the guard stays off.
const LIVE_HOLD: Duration = Duration::from_secs(3);

/// Whether the shared state allows rendering ahead right now.
fn shared_allows_render_ahead(shared: &SharedState) -> bool {
    let state = AudioEngineState::from_u8(shared.engine_state.load(Ordering::Relaxed));
    shared.playing.load(Ordering::Relaxed)
        && state.renders_transport()
        && !shared.recording_active.load(Ordering::Relaxed)
        && !shared.monitor_enabled_any.load(Ordering::Relaxed)
        && shared.count_in_remaining_samples.load(Ordering::Relaxed) == 0
        && shared.jam_bus.master_publish().is_none()
        && shared.jam_bus.live_input_publish().is_none()
        && shared.jam_bus.monitor_publish().is_none()
}

fn guard_thread(inner: Arc<GuardInner>) {
    flush_denormals_on_this_thread();
    crate::parallel::promote_audio_thread();
    let shared = Arc::clone(&inner.shared);
    let channels = inner.channels;
    let mut seen_live_events = shared.live_events.load(Ordering::Relaxed);
    let mut live_until: Option<Instant> = None;
    loop {
        if inner.stop.load(Ordering::Acquire) {
            break;
        }
        // Played notes and controllers hold render-ahead off for a while, so
        // a performance is heard as it is played.
        let live_events = shared.live_events.load(Ordering::Relaxed);
        if live_events != seen_live_events {
            seen_live_events = live_events;
            live_until = Some(Instant::now() + LIVE_HOLD);
        }
        let live_hold = live_until.is_some_and(|until| Instant::now() < until);
        let level =
            AudioguardLevel::from_u8(shared.dropout_protection_mode.load(Ordering::Relaxed));
        let block = inner.block_frames.load(Ordering::Relaxed);
        let sample_rate = shared.sample_rate.load(Ordering::Relaxed);
        let target = if block > 0 && block <= MAX_GUARD_BLOCK_FRAMES {
            level.render_ahead_frames(block, sample_rate)
        } else {
            0
        };
        let allowed = target > 0
            && !live_hold
            && !inner.failed.load(Ordering::Relaxed)
            && shared_allows_render_ahead(&shared);
        let pending = shared.guard_pending_frames.load(Ordering::Acquire) as usize;
        let room = inner.ring.len() < GUARD_SLOTS - 1;
        if allowed && room && pending < target {
            let mut core = inner.core.lock();
            if core.has_live_tracks() {
                drop(core);
                shared.guard_engaged.store(false, Ordering::Relaxed);
                std::thread::park_timeout(Duration::from_millis(5));
                continue;
            }
            // Counted as pending before it renders, so the playhead keeps
            // reading the played cursor while the graph's cursor moves on. The
            // first block ahead starts from where the device is.
            if shared.guard_pending_frames.load(Ordering::Acquire) == 0 {
                shared.guard_played_cursor.store(
                    shared.position_samples.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );
            }
            shared
                .guard_pending_frames
                .fetch_add(block as u64, Ordering::AcqRel);
            let tail = inner.ring.tail.load(Ordering::Relaxed);
            // SAFETY: `tail - head < GUARD_SLOTS - 1` (checked above, and the
            // consumer only ever moves `head` forward), so the consumer is not
            // reading this slot; only this thread writes slots.
            let slot = unsafe { &mut *inner.ring.slots[tail % GUARD_SLOTS].get() };
            let samples = &mut slot.samples[..block * channels];
            core.local.guard_rendering = true;
            let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::plugin_bridge::set_render_ahead_grace(block_period(block, &shared));
                core.render(samples, channels, &shared);
                crate::plugin_bridge::set_render_ahead_grace(Duration::ZERO);
            }));
            core.local.guard_rendering = false;
            if rendered.is_err() {
                crate::plugin_bridge::set_render_ahead_grace(Duration::ZERO);
                shared
                    .guard_pending_frames
                    .fetch_sub(block as u64, Ordering::AcqRel);
                inner.failed.store(true, Ordering::Relaxed);
                shared.guard_engaged.store(false, Ordering::Relaxed);
                eprintln!(
                    "[Audioguard] a render ahead panicked; rendering in the callback from now on"
                );
                continue;
            }
            slot.frames = block;
            slot.start = core.local.last_block_start;
            slot.playing = core.local.last_block_playing;
            slot.next_position = shared.position_samples.load(Ordering::Relaxed);
            drop(core);
            inner
                .ring
                .tail
                .store(tail.wrapping_add(1), Ordering::Release);
            shared.guard_engaged.store(true, Ordering::Relaxed);
            shared
                .guard_ahead_frames
                .store(target as u64, Ordering::Relaxed);
            continue;
        }
        if !allowed {
            shared.guard_engaged.store(false, Ordering::Relaxed);
        }
        // Woken by every device block; the timeout only covers a stalled
        // device.
        std::thread::park_timeout(Duration::from_millis(if allowed { 2 } else { 10 }));
    }
    shared.guard_engaged.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_round_trip_and_old_modes_map_to_normal() {
        assert_eq!(AudioguardLevel::from_u8(0), AudioguardLevel::Off);
        assert_eq!(AudioguardLevel::from_u8(1), AudioguardLevel::Normal);
        assert_eq!(AudioguardLevel::from_u8(2), AudioguardLevel::Normal);
        assert_eq!(AudioguardLevel::from_u8(3), AudioguardLevel::High);
        for level in [
            AudioguardLevel::Off,
            AudioguardLevel::Normal,
            AudioguardLevel::High,
        ] {
            assert_eq!(AudioguardLevel::from_u8(level as u8), level);
        }
    }

    #[test]
    fn render_ahead_is_whole_blocks_and_fits_the_ring() {
        let normal = AudioguardLevel::Normal.render_ahead_frames(256, 48_000);
        // 12 ms at 48 kHz is 576 frames: three 256-frame blocks.
        assert_eq!(normal, 768);
        let high = AudioguardLevel::High.render_ahead_frames(256, 48_000);
        assert_eq!(high % 256, 0);
        assert!(high >= 1920);
        assert_eq!(AudioguardLevel::Off.render_ahead_frames(256, 48_000), 0);
        let tiny = AudioguardLevel::High.render_ahead_frames(32, 192_000);
        assert!(tiny <= (GUARD_SLOTS - 2) * 32);
    }

    #[test]
    fn the_output_guard_mutes_invalid_samples_and_bursts_then_fades_back() {
        let shared = SharedState::default();
        let mut guard = OutputGuard::default();
        let mut quiet = vec![0.5f32; 64];
        guard.process(&mut quiet, 2, 48_000, &shared);
        assert!(
            quiet.iter().all(|&s| s == 0.5),
            "a normal block passes untouched"
        );

        let mut bad = vec![0.5f32; 64];
        bad[7] = f32::NAN;
        guard.process(&mut bad, 2, 48_000, &shared);
        assert!(bad.iter().all(|&s| s == 0.0));
        assert_eq!(shared.guard_output_mutes.load(Ordering::Relaxed), 1);
        assert_eq!(
            OutputGuardReason::from_u8(shared.guard_output_reason.load(Ordering::Relaxed)),
            OutputGuardReason::InvalidSamples
        );

        // Muted for half a second, whatever plays.
        let mut next = vec![0.5f32; 2 * 1000];
        guard.process(&mut next, 2, 48_000, &shared);
        assert!(next.iter().all(|&s| s == 0.0));

        let mut burst = vec![0.0f32; 64];
        burst[3] = 8.0;
        guard.process(&mut burst, 2, 48_000, &shared);
        assert_eq!(
            OutputGuardReason::from_u8(shared.guard_output_reason.load(Ordering::Relaxed)),
            OutputGuardReason::Burst
        );

        // Past the mute, the level comes back through a fade.
        let mut long = vec![0.5f32; 2 * 48_000];
        guard.process(&mut long, 2, 48_000, &shared);
        let mute_frames = (GUARD_MUTE_SECONDS * 48_000.0) as usize;
        assert_eq!(long[0], 0.0);
        assert!(long[2 * (mute_frames + 10)] > 0.0 && long[2 * (mute_frames + 10)] < 0.5);
        assert_eq!(long[long.len() - 1], 0.5);
    }

    fn playing_shared() -> Arc<SharedState> {
        let shared = Arc::new(SharedState::default());
        shared.sample_rate.store(48_000, Ordering::Relaxed);
        shared
            .engine_state
            .store(AudioEngineState::Running as u8, Ordering::Relaxed);
        shared.playing.store(true, Ordering::Relaxed);
        shared.test_tone_enabled.store(true, Ordering::Relaxed);
        shared
            .test_tone_freq
            .store(crate::engine::f32_store(441.0), Ordering::Relaxed);
        shared
            .dropout_protection_mode
            .store(AudioguardLevel::Normal as u8, Ordering::Relaxed);
        shared
    }

    fn playing_core() -> (RenderCore, crossbeam_channel::Sender<EngineCommand>) {
        let runtime = RuntimeProject {
            sample_rate: 48_000,
            ..Default::default()
        };
        let mut local = LocalAudioState::new(48_000.0);
        local.playing_local = true;
        local.set_metronome_enabled(true, 0, 48_000);
        let (tx, rx) = crossbeam_channel::bounded(16);
        (RenderCore::new(runtime, local, rx, 48_000), tx)
    }

    /// Rendering ahead must not change a single sample: the device hears
    /// exactly what rendering every block in the callback would have played,
    /// and the transport ends in the same place.
    #[test]
    fn render_ahead_plays_exactly_what_inline_rendering_plays() {
        const BLOCK: usize = 256;
        const BLOCKS: usize = 120;

        let reference_shared = playing_shared();
        let (mut reference, _keep) = playing_core();
        let mut expected = Vec::with_capacity(BLOCK * 2 * BLOCKS);
        for _ in 0..BLOCKS {
            let mut block = vec![0.0f32; BLOCK * 2];
            reference.render(&mut block, 2, &reference_shared);
            expected.extend_from_slice(&block);
        }

        let shared = playing_shared();
        let (core, _keep_tx) = playing_core();
        let mut guarded = GuardedRenderer::new(core, 2, Arc::clone(&shared));
        let mut played = Vec::with_capacity(BLOCK * 2 * BLOCKS);
        let mut saw_ahead = false;
        let mut playhead_behind = false;
        for _ in 0..BLOCKS {
            // Time between device callbacks, during which the guard runs ahead.
            std::thread::sleep(Duration::from_millis(1));
            let mut block = vec![0.0f32; BLOCK * 2];
            guarded.render(&mut block);
            played.extend_from_slice(&block);
            if shared.guard_pending_frames.load(Ordering::Acquire) > 0 {
                saw_ahead = true;
                if shared.playback_position() < shared.position_samples.load(Ordering::Relaxed) {
                    playhead_behind = true;
                }
            }
        }
        drop(guarded);

        assert!(saw_ahead, "the guard never rendered ahead");
        assert!(
            playhead_behind,
            "while blocks wait in the ring, the playhead reads what is played, not the render cursor"
        );
        assert_eq!(shared.guard_misses.load(Ordering::Relaxed), 0);
        assert_eq!(played.len(), expected.len());
        let first_difference = played
            .iter()
            .zip(&expected)
            .position(|(a, b)| a.to_bits() != b.to_bits());
        assert_eq!(first_difference, None, "render-ahead changed the audio");
    }

    /// Anything live stops the guard producing, the ring plays out, and the
    /// callback renders every block itself again — still without a gap.
    #[test]
    fn a_monitored_input_hands_rendering_back_to_the_callback() {
        const BLOCK: usize = 256;
        let shared = playing_shared();
        let (core, _tx) = playing_core();
        let mut guarded = GuardedRenderer::new(core, 2, Arc::clone(&shared));
        let mut block = vec![0.0f32; BLOCK * 2];
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(1));
            guarded.render(&mut block);
        }
        assert!(shared.guard_pending_frames.load(Ordering::Acquire) > 0);

        shared.monitor_enabled_any.store(true, Ordering::Relaxed);
        let mut drained = false;
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(1));
            guarded.render(&mut block);
            if shared.guard_pending_frames.load(Ordering::Acquire) == 0 {
                drained = true;
            }
        }
        assert!(drained, "the ring never played out");
        assert_eq!(shared.guard_pending_frames.load(Ordering::Acquire), 0);
        assert!(!shared.guard_engaged.load(Ordering::Relaxed));
        assert_eq!(
            shared.playback_position(),
            shared.position_samples.load(Ordering::Relaxed)
        );
        assert_eq!(shared.guard_misses.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn switching_a_bypassed_insert_back_on_rearms_it() {
        let mut guard = InsertGuard::default();
        let shared_health = Arc::clone(&guard.health);
        for _ in 0..INVALID_RUN_TO_TRIP {
            let mut left = vec![f32::NAN; 8];
            let mut right = vec![0.0f32; 8];
            guard.check_output(&mut left, &mut right);
        }
        assert!(guard.bypassed);
        shared_health.reported.store(true, Ordering::Relaxed);
        guard.rearm();
        assert!(!guard.bypassed);
        assert_eq!(
            InsertTrip::from_u8(shared_health.tripped.load(Ordering::Relaxed)),
            InsertTrip::None
        );
        assert!(!shared_health.reported.load(Ordering::Relaxed));
        // Watched afresh: one bad block alone does not trip it again.
        let mut left = vec![f32::NAN; 8];
        let mut right = vec![0.0f32; 8];
        guard.check_output(&mut left, &mut right);
        assert!(!guard.bypassed);
    }

    #[test]
    fn an_insert_that_keeps_writing_nan_is_bypassed() {
        let mut guard = InsertGuard::default();
        let mut left = vec![0.1f32; 64];
        let mut right = vec![0.1f32; 64];
        guard.check_output(&mut left, &mut right);
        assert!(!guard.bypassed);
        for round in 0..INVALID_RUN_TO_TRIP {
            left[5] = f32::NAN;
            guard.check_output(&mut left, &mut right);
            assert!(
                left.iter().all(|&s| s == 0.0),
                "the bad block is silenced at once"
            );
            assert_eq!(guard.bypassed, round + 1 == INVALID_RUN_TO_TRIP);
        }
        assert_eq!(
            InsertTrip::from_u8(guard.health.tripped.load(Ordering::Relaxed)),
            InsertTrip::InvalidOutput
        );
        // A single bad block among good ones does not trip a fresh insert.
        let mut fresh = InsertGuard::default();
        let mut l = vec![f32::INFINITY; 8];
        let mut r = vec![0.0f32; 8];
        fresh.check_output(&mut l, &mut r);
        let mut l = vec![0.2f32; 8];
        fresh.check_output(&mut l, &mut r);
        assert!(!fresh.bypassed);
    }

    /// High bypasses a bridged plug-in only for misses on the device
    /// callback. Render-ahead misses are the host still finishing a block
    /// the guard asked for immediately; counting them turned every effect
    /// off as soon as High was rendering ahead.
    #[test]
    fn high_does_not_bypass_for_misses_while_rendering_ahead() {
        let mut guard = InsertGuard::default();
        for _ in 0..(MISS_WINDOW_BLOCKS * 2) {
            guard.note_bridge_block_at(true, AudioguardLevel::High, true);
        }
        assert!(!guard.bypassed);
        assert_eq!(guard.window_blocks, 0);
        assert_eq!(guard.window_misses, 0);

        let mut normal = InsertGuard::default();
        for _ in 0..(MISS_WINDOW_BLOCKS * 2) {
            normal.note_bridge_block_at(true, AudioguardLevel::Normal, false);
        }
        assert!(!normal.bypassed);

        let mut late = InsertGuard::default();
        let trip_at = MISS_WINDOW_BLOCKS / MISS_SHARE_TO_TRIP;
        for block in 0..MISS_WINDOW_BLOCKS {
            late.note_bridge_block_at(block < trip_at, AudioguardLevel::High, false);
        }
        assert!(
            late.bypassed,
            "a plug-in missing one device block in four is bypassed"
        );

        let mut jitter = InsertGuard::default();
        for block in 0..MISS_WINDOW_BLOCKS {
            jitter.note_bridge_block_at(block + 1 < trip_at, AudioguardLevel::High, false);
        }
        assert!(
            !jitter.bypassed,
            "fewer than one miss in four stays in the chain"
        );
    }

    #[test]
    fn denormal_scope_flushes_and_restores() {
        let before = read_fp_control_for_test();
        {
            let _scope = DenormalScope::enter();
            let tiny = std::hint::black_box(1.0e-38f32);
            let product = std::hint::black_box(tiny * 1.0e-3);
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            assert_eq!(product, 0.0, "a denormal result is flushed to zero");
            let _ = product;
        }
        assert_eq!(read_fp_control_for_test(), before);
    }

    fn read_fp_control_for_test() -> u64 {
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            read_fp_control()
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            0
        }
    }
}

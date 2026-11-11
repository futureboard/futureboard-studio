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
//!   back to rendering every block itself, so a monitored input never waits
//!   behind the ring. A played note is only seen when its command is drained
//!   by a render: the first note after a pause can sit behind what the ring
//!   already holds (at most the render-ahead depth); the ones after it do not.
//! * **Denormal flushing.** Every thread that renders runs with flush-to-zero
//!   and denormals-are-zero set, so a reverb or filter tail decaying into the
//!   denormal range does not cost a hundred times its normal CPU.
//! * **Output guard.** The device output is muted for a moment when it carries
//!   invalid samples (NaN / infinity) or a burst far beyond full scale — a
//!   feedback loop or a broken plug-in — instead of sending it to the speakers.
//!
//! Ownership: [`RenderCore`] holds everything the render owns (the graph, the
//! per-stream state and the command queue). It sits behind a mutex the device
//! callback and the guard thread only `try_lock`, so neither waits on it and
//! the callback's unlock never has a parked waiter to wake. Whoever holds it renders the next block at the
//! graph's cursor. The guard thread publishes each block to the ring before it
//! lets go of the core, so a callback holding the core sees every block
//! rendered ahead, plays those first and only then renders inline: blocks are
//! heard in render order. Each block carries its render index, and the
//! callback counts a miss for any block that is not the next one in order
//! (none should be) as well as for a block that is not ready in time.
//!
//! What the callback does: a copy from the ring, or an inline render under a
//! `try_lock`. When the guard holds the core, the callback busy-polls for at
//! most half a period, then plays silence and counts a miss; the late block is
//! played next, in order. It never allocates, logs or waits on a lock, and it
//! wakes the guard thread (`Thread::unpark`) once per block.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
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
static PROCESS_LEVEL: AtomicU8 = AtomicU8::new(AudioguardLevel::Normal as u8);

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
///
/// One atomic holds both the trip and whether it was reported, so the render
/// re-arming an insert and the control thread reporting it cannot interleave
/// into "reported, not tripped" — which hid the insert's next trip for good.
#[derive(Debug, Default)]
pub struct InsertHealth {
    /// An [`InsertTrip`] as `u8`, with [`Self::REPORTED`] set once the control
    /// side has told the studio.
    state: AtomicU8,
}

impl InsertHealth {
    const REPORTED: u8 = 0x80;

    /// Why the insert is bypassed, reported or not.
    pub fn trip(&self) -> InsertTrip {
        InsertTrip::from_u8(self.state.load(Ordering::Relaxed) & !Self::REPORTED)
    }

    /// Render side: a store, never a wait.
    #[inline]
    fn set(&self, trip: InsertTrip) {
        self.state.store(trip as u8, Ordering::Relaxed);
    }

    /// The trip nobody has been told about yet, marked told. Control side.
    /// `None` when there is none, or when the render changed it meanwhile
    /// (the next poll reads the new state).
    fn take_unreported(&self) -> Option<InsertTrip> {
        let state = self.state.load(Ordering::Relaxed);
        let trip = InsertTrip::from_u8(state);
        if trip == InsertTrip::None {
            // Not tripped, or already reported (`from_u8` maps both to None).
            return None;
        }
        self.state
            .compare_exchange(
                state,
                state | Self::REPORTED,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok()
            .map(|_| trip)
    }
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
        // Read once: `overruns` must hold the same misses `ring_misses` shows.
        let ring_misses = shared.guard_misses.load(Ordering::Relaxed);
        Self {
            level: AudioguardLevel::from_u8(shared.dropout_protection_mode.load(Ordering::Relaxed)),
            rendering_ahead: engaged,
            ahead_ms: if engaged {
                shared.guard_ahead_frames.load(Ordering::Relaxed) as f32 * 1000.0 / rate
            } else {
                0.0
            },
            ring_misses,
            overruns: shared
                .callback_overruns
                .load(Ordering::Relaxed)
                .saturating_add(ring_misses)
                .saturating_add(shared.device_xruns.load(Ordering::Relaxed)),
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
            let Some(reason) = insert.guard.health.take_unreported() else {
                continue;
            };
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
    /// Times a bypass lane re-armed it since the user last switched it on.
    lane_rearms: u8,
}

/// How often a bypass lane may switch a guard-bypassed insert back on before
/// it takes the user's own switch. A lane inside a loop would otherwise run a
/// broken plug-in, and report its bypass, on every pass.
const LANE_REARMS: u8 = 1;

/// Consecutive invalid blocks before an insert is bypassed.
const INVALID_RUN_TO_TRIP: u8 = 3;
/// Blocks per miss-rate window for bridged plug-ins, and the share of misses
/// (1 in `MISS_SHARE_TO_TRIP`) that trips it.
const MISS_WINDOW_BLOCKS: u32 = 512;
const MISS_SHARE_TO_TRIP: u32 = 4;

impl InsertGuard {
    /// Process again and watch afresh: the user switched the insert back on.
    /// Realtime-safe: stores only. The health is cleared in place (it is
    /// shared with the control mirror; a new one would allocate).
    pub(crate) fn rearm(&mut self) {
        self.lane_rearms = 0;
        self.watch_afresh();
    }

    /// The insert's bypass lane switched it back on. Lifts a bypass of the
    /// guard's, [`LANE_REARMS`] times; an insert the guard has not bypassed is
    /// left as it is, so the lane cannot keep resetting what is being counted
    /// against it. Realtime-safe: stores only.
    pub(crate) fn rearm_from_lane(&mut self) {
        if !self.bypassed || self.lane_rearms >= LANE_REARMS {
            return;
        }
        self.lane_rearms += 1;
        self.watch_afresh();
    }

    fn watch_afresh(&mut self) {
        self.bypassed = false;
        self.invalid_run = 0;
        self.window_blocks = 0;
        self.window_misses = 0;
        self.health.set(InsertTrip::None);
    }

    fn trip(&mut self, why: InsertTrip) {
        self.bypassed = true;
        self.health.set(why);
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
    /// Renders started so far, by anyone. The index of the next render is the
    /// order the device must hear it in: a ring slot carries the index it was
    /// rendered at, and the callback checks it plays them in sequence.
    blocks_rendered: u64,
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
            blocks_rendered: 0,
        }
    }

    /// Drain the command queue and render one block into `out`. Counted
    /// before it runs, so a render that panics leaves a gap in the sequence.
    #[inline]
    fn render(&mut self, out: &mut [f32], channels: usize, shared: &Arc<SharedState>) {
        self.blocks_rendered = self.blocks_rendered.wrapping_add(1);
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
    /// [`RenderCore::blocks_rendered`] before this block was rendered.
    sequence: u64,
}

/// Single-producer (guard thread) / single-consumer (device callback) ring of
/// rendered blocks. Slots are allocated once; nothing is allocated after.
///
/// The producer publishes a block (stores `tail`) while it still holds the
/// [`RenderCore`] lock. So whoever holds that lock sees every block rendered
/// ahead already in the ring, and the graph's cursor is right after the last
/// of them. The callback relies on this to render inline in order.
struct GuardRing {
    slots: Box<[UnsafeCell<Slot>]>,
    /// Blocks consumed / published, monotonically counted.
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: a slot is written only by the producer (the guard thread) while
// `tail - head < GUARD_SLOTS - 1` says the consumer is not reading it, and read
// only by the consumer (the device callback) between the producer's Release
// store of `tail` and its own Release store of `head`.
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
                    sequence: 0,
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
    /// The stream's own rate ([`RenderCore::sample_rate`]). Not the shared
    /// one: that is published only after the stream is open, so its first
    /// blocks would size the ring and the callback's wait from the previous
    /// stream's rate, or from zero.
    sample_rate: u32,
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
    /// Render index ([`RenderCore::blocks_rendered`]) the device must hear
    /// next. A block that arrives with another index is out of order.
    next_block: u64,
}

impl GuardedRenderer {
    /// Wrap a stream's render state. `channels` is the device's interleaved
    /// channel count; it does not change for the life of the stream.
    pub fn new(core: RenderCore, channels: usize, shared: Arc<SharedState>) -> Self {
        let channels = channels.max(1);
        let sample_rate = core.sample_rate;
        let inner = Arc::new(GuardInner {
            core: Mutex::new(core),
            ring: GuardRing::new(channels),
            channels,
            sample_rate,
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
            next_block: 0,
        }
    }

    /// Fill one interleaved device block, in render order.
    ///
    /// Plays what the ring holds; for what is still missing it takes the
    /// render state with one `try_lock` and renders inline. When the guard
    /// holds the render state it is rendering the very block this needs: the
    /// callback then polls the ring and `try_lock` for at most half a period,
    /// and fills silence and counts a miss if the block has not come. It never
    /// waits on the lock, allocates or logs; it does wake the guard thread
    /// (`Thread::unpark`) once per block.
    pub fn render(&mut self, out: &mut [f32]) {
        let Self {
            inner,
            thread,
            front_offset,
            next_block,
        } = self;
        let inner = &**inner;
        let channels = inner.channels;
        let shared = &inner.shared;
        let need = out.len() / channels;
        // Whole frames only. A ragged tail (no backend passes one) is silence
        // rather than whatever the buffer held when the ring supplies the block.
        let (out, tail) = out.split_at_mut(need * channels);
        tail.fill(0.0);
        inner.block_frames.store(need, Ordering::Relaxed);
        let mut filled = take_from_ring(inner, front_offset, next_block, out, 0, need);
        let mut wait_until = None;
        while filled < need {
            if let Some(mut core) = inner.core.try_lock() {
                // The guard publishes a block before it lets go of the render
                // state, so with it held the ring holds every block rendered
                // ahead. Play those first; what is still missing starts at the
                // graph's cursor.
                filled = take_from_ring(inner, front_offset, next_block, out, filled, need);
                if filled < need {
                    let index = core.blocks_rendered;
                    if index != *next_block {
                        count_miss(shared);
                    }
                    core.render(&mut out[filled * channels..], channels, shared);
                    *next_block = index.wrapping_add(1);
                    filled = need;
                }
                break;
            }
            // The guard holds the render state: it is rendering the block this
            // needs. Give it part of the period, then fall back to silence.
            let now = Instant::now();
            let until =
                *wait_until.get_or_insert_with(|| now + block_period(need, inner.sample_rate) / 2);
            if now >= until {
                break;
            }
            std::hint::spin_loop();
            filled = take_from_ring(inner, front_offset, next_block, out, filled, need);
        }
        if filled < need {
            out[filled * channels..].fill(0.0);
            count_miss(shared);
        }
        if let Some(thread) = thread {
            thread.thread().unpark();
        }
    }
}

/// A device block the guard did not deliver in time or in order.
#[inline]
fn count_miss(shared: &SharedState) {
    shared.dropout_count.fetch_add(1, Ordering::Relaxed);
    shared.guard_misses.fetch_add(1, Ordering::Relaxed);
    shared
        .dropout_last_reason
        .store(DropoutReason::RenderAheadMiss as u8, Ordering::Relaxed);
}

/// Copy frames `from..need` of `out` from the ring. Returns how far `out` is
/// filled after it. Publishes the played block clock as it goes, and counts a
/// miss for a block that is not the next in render order. Consumer side only
/// (the device callback).
fn take_from_ring(
    inner: &GuardInner,
    front_offset: &mut usize,
    next_block: &mut u64,
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
        if *front_offset == 0 {
            if slot.sequence != *next_block {
                count_miss(shared);
            }
            *next_block = slot.sequence.wrapping_add(1);
        }
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
fn block_period(frames: usize, sample_rate: u32) -> Duration {
    Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(sample_rate.max(1)))
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

/// Waits of the guard thread. The device callback unparks it once per block,
/// so these only bound how long it sleeps through a stalled device, or through
/// a change of state nothing wakes it for.
const WAIT_RING_FULL: Duration = Duration::from_millis(2);
const WAIT_CORE_BUSY: Duration = Duration::from_millis(2);
const WAIT_LIVE_TRACKS: Duration = Duration::from_millis(5);
const WAIT_NOT_ALLOWED: Duration = Duration::from_millis(10);

/// Holds render-ahead off for [`LIVE_HOLD`] after each played note or
/// controller, so a performance is heard as it is played.
struct LiveHold {
    seen_events: u64,
    until: Option<Instant>,
}

impl LiveHold {
    fn new(shared: &SharedState) -> Self {
        Self {
            seen_events: shared.live_events.load(Ordering::Relaxed),
            until: None,
        }
    }

    /// Whether someone played within the hold. Call once per guard pass.
    fn poll(&mut self, shared: &SharedState) -> bool {
        let events = shared.live_events.load(Ordering::Relaxed);
        if events != self.seen_events {
            self.seen_events = events;
            self.until = Some(Instant::now() + LIVE_HOLD);
        }
        self.until.is_some_and(|until| Instant::now() < until)
    }
}

impl GuardInner {
    /// The device block size and the ring depth to keep, both in frames, or
    /// `None` while render-ahead is off: the level, the transport, something
    /// live, a block too large to hold, or an earlier failure.
    fn render_ahead_plan(&self, live_hold: bool) -> Option<(usize, usize)> {
        if live_hold
            || self.failed.load(Ordering::Relaxed)
            || !shared_allows_render_ahead(&self.shared)
        {
            return None;
        }
        let block = self.block_frames.load(Ordering::Relaxed);
        if block == 0 || block > MAX_GUARD_BLOCK_FRAMES {
            return None;
        }
        let level =
            AudioguardLevel::from_u8(self.shared.dropout_protection_mode.load(Ordering::Relaxed));
        let target = level.render_ahead_frames(block, self.sample_rate);
        (target > 0).then_some((block, target))
    }

    /// Render one `block`-frame block ahead into the ring, unless the ring
    /// already holds `target` frames. Returns how long to wait before the next
    /// pass, or `None` to go straight on to the next block. Guard thread only:
    /// it is the ring's one producer.
    fn render_block_ahead(&self, block: usize, target: usize) -> Option<Duration> {
        let shared = &self.shared;
        let pending = shared.guard_pending_frames.load(Ordering::Acquire) as usize;
        if self.ring.len() >= GUARD_SLOTS - 1 || pending >= target {
            return Some(WAIT_RING_FULL);
        }
        // Never wait on the lock: a waiter parked on it would make the
        // callback's unlock wake it from the audio thread. The callback holds
        // it only while it renders a block, and unparks this thread after.
        let Some(mut core) = self.core.try_lock() else {
            return Some(WAIT_CORE_BUSY);
        };
        if core.has_live_tracks() {
            drop(core);
            shared.guard_engaged.store(false, Ordering::Relaxed);
            return Some(WAIT_LIVE_TRACKS);
        }
        // Counted as pending before it renders, so the playhead keeps reading
        // the played cursor while the graph's cursor moves on. The first block
        // ahead starts from where the device is.
        if shared.guard_pending_frames.load(Ordering::Acquire) == 0 {
            shared.guard_played_cursor.store(
                shared.position_samples.load(Ordering::Relaxed),
                Ordering::Relaxed,
            );
        }
        shared
            .guard_pending_frames
            .fetch_add(block as u64, Ordering::AcqRel);
        let tail = self.ring.tail.load(Ordering::Relaxed);
        // SAFETY: `tail - head < GUARD_SLOTS - 1` was checked above, this
        // thread is the only one that moves `tail`, and the consumer only ever
        // moves `head` forward: the consumer is not reading this slot, and
        // nothing else writes slots.
        let slot = unsafe { &mut *self.ring.slots[tail % GUARD_SLOTS].get() };
        let samples = &mut slot.samples[..block * self.channels];
        let sequence = core.blocks_rendered;
        core.local.guard_rendering = true;
        crate::plugin_bridge::set_render_ahead_grace(block_period(block, self.sample_rate));
        // Only catches in builds that unwind. The release profile aborts on
        // panic, there as in the device callback.
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            core.render(samples, self.channels, shared);
        }));
        crate::plugin_bridge::set_render_ahead_grace(Duration::ZERO);
        core.local.guard_rendering = false;
        if rendered.is_err() {
            shared
                .guard_pending_frames
                .fetch_sub(block as u64, Ordering::AcqRel);
            self.failed.store(true, Ordering::Relaxed);
            shared.guard_engaged.store(false, Ordering::Relaxed);
            eprintln!(
                "[Audioguard] a render ahead panicked; rendering in the callback from now on"
            );
            return None;
        }
        slot.frames = block;
        slot.start = core.local.last_block_start;
        slot.playing = core.local.last_block_playing;
        slot.next_position = shared.position_samples.load(Ordering::Relaxed);
        slot.sequence = sequence;
        // Published before the render state is let go. Unlocking first left a
        // window in which the ring looked empty to a callback that already
        // held the render state: it rendered the next block inline and played
        // this one after it (audit #408).
        self.ring
            .tail
            .store(tail.wrapping_add(1), Ordering::Release);
        drop(core);
        shared.guard_engaged.store(true, Ordering::Relaxed);
        shared
            .guard_ahead_frames
            .store(target as u64, Ordering::Relaxed);
        None
    }
}

fn guard_thread(inner: Arc<GuardInner>) {
    flush_denormals_on_this_thread();
    crate::parallel::promote_audio_thread();
    let shared = &inner.shared;
    let mut live = LiveHold::new(shared);
    while !inner.stop.load(Ordering::Acquire) {
        let live_hold = live.poll(shared);
        let wait = match inner.render_ahead_plan(live_hold) {
            Some((block, target)) => inner.render_block_ahead(block, target),
            None => {
                shared.guard_engaged.store(false, Ordering::Relaxed);
                Some(WAIT_NOT_ALLOWED)
            }
        };
        if let Some(wait) = wait {
            std::thread::park_timeout(wait);
        }
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

    /// The reported case (audit #408): device blocks asked for back-to-back,
    /// with no idle time between callbacks, which is when the callback and
    /// the guard thread contend for the render on almost every block. The
    /// output must be exactly what inline rendering plays; a block that comes
    /// late or out of order must be counted as a miss. Several runs, because
    /// it is a race.
    #[test]
    fn back_to_back_render_ahead_plays_in_order_or_counts_a_miss() {
        const BLOCK: usize = 256;
        const BLOCKS: usize = 3000;
        const RUNS: usize = 6;

        let reference_shared = playing_shared();
        let (mut reference, _keep) = playing_core();
        let mut expected = vec![0.0f32; BLOCK * 2 * BLOCKS];
        for block in expected.chunks_mut(BLOCK * 2) {
            reference.render(block, 2, &reference_shared);
        }

        let mut failures = Vec::new();
        let mut ring_blocks = 0usize;
        let mut clean_runs = 0usize;
        for run in 0..RUNS {
            let shared = playing_shared();
            let (core, _keep_tx) = playing_core();
            let mut guarded = GuardedRenderer::new(core, 2, Arc::clone(&shared));
            let mut played = vec![0.0f32; BLOCK * 2 * BLOCKS];
            for block in played.chunks_mut(BLOCK * 2) {
                guarded.render(block);
                if shared.guard_pending_frames.load(Ordering::Acquire) > 0 {
                    ring_blocks += 1;
                }
            }
            drop(guarded);
            let misses = shared.guard_misses.load(Ordering::Relaxed);
            let first_difference = played
                .iter()
                .zip(&expected)
                .position(|(a, b)| a.to_bits() != b.to_bits());
            println!(
                "run {run}: first difference at block {:?}, {misses} misses",
                first_difference.map(|sample| sample / (BLOCK * 2))
            );
            if first_difference.is_none() && misses == 0 {
                clean_runs += 1;
            }
            if let Some(sample) = first_difference {
                if misses == 0 {
                    let at = sample / (BLOCK * 2);
                    // Which inline block was played there instead, if any.
                    fn block_of(audio: &[f32], index: usize) -> Option<&[f32]> {
                        audio.get(index * BLOCK * 2..(index + 1) * BLOCK * 2)
                    }
                    let played_instead = (at.saturating_sub(4)..(at + 4).min(BLOCKS))
                        .find(|&other| block_of(&expected, other) == block_of(&played, at));
                    failures.push(format!(
                        "run {run}: block {at} differs from inline rendering (it holds inline \
                         block {played_instead:?}) with 0 misses counted"
                    ));
                }
            }
        }
        assert!(ring_blocks > 0, "the guard never rendered ahead");
        assert!(
            failures.is_empty(),
            "{} of {RUNS} runs played blocks out of order without a miss:\n{}",
            failures.len(),
            failures.join("\n")
        );
        // A miss is only a guard render that took longer than half a period,
        // which a test tone does not: most runs must be exact, not excused.
        assert!(
            clean_runs * 2 > RUNS,
            "only {clean_runs} of {RUNS} runs matched inline rendering without a miss"
        );
    }

    /// A block that reaches the callback out of render order is counted as a
    /// miss, whatever let it through.
    #[test]
    fn a_block_out_of_render_order_is_counted_as_a_miss() {
        let shared = playing_shared();
        let (core, _tx) = playing_core();
        let inner = GuardInner {
            core: Mutex::new(core),
            ring: GuardRing::new(2),
            channels: 2,
            sample_rate: 48_000,
            shared: Arc::clone(&shared),
            block_frames: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        };
        // Publish render 1 while the device is waiting for render 0.
        for (index, sequence) in [(0usize, 1u64), (1, 2)] {
            // SAFETY: no consumer is reading yet; this test is the producer.
            let slot = unsafe { &mut *inner.ring.slots[index].get() };
            slot.frames = 4;
            slot.sequence = sequence;
        }
        inner.ring.tail.store(2, Ordering::Release);
        let mut front_offset = 0;
        let mut next_block = 0;
        let mut out = vec![1.0f32; 8 * 2];
        let filled = take_from_ring(&inner, &mut front_offset, &mut next_block, &mut out, 0, 8);
        assert_eq!(filled, 8);
        assert_eq!(next_block, 3);
        assert_eq!(
            shared.guard_misses.load(Ordering::Relaxed),
            1,
            "the skipped render is one miss; the block after it is in order"
        );
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
        // Until the guard is seen ahead, not at one fixed instant: with the
        // whole suite running, the guard thread can be late to its turn.
        let mut ahead = false;
        for _ in 0..400 {
            std::thread::sleep(Duration::from_millis(1));
            guarded.render(&mut block);
            if shared.guard_pending_frames.load(Ordering::Acquire) > 0 {
                ahead = true;
                break;
            }
        }
        assert!(ahead, "the guard never rendered ahead");

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
        assert_eq!(
            shared_health.take_unreported(),
            Some(InsertTrip::InvalidOutput)
        );
        assert_eq!(shared_health.take_unreported(), None, "reported once");
        assert_eq!(shared_health.trip(), InsertTrip::InvalidOutput);
        guard.rearm();
        assert!(!guard.bypassed);
        assert_eq!(shared_health.trip(), InsertTrip::None);
        assert_eq!(shared_health.take_unreported(), None);
        // Watched afresh: one bad block alone does not trip it again.
        let mut left = vec![f32::NAN; 8];
        let mut right = vec![0.0f32; 8];
        guard.check_output(&mut left, &mut right);
        assert!(!guard.bypassed);
    }

    /// A bypass lane lifts the guard's bypass once; after that the insert
    /// stays out until the user switches it on. And a lane edge never touches
    /// an insert the guard has not bypassed.
    #[test]
    fn a_lane_rearms_a_bypassed_insert_once_and_leaves_a_watched_one_alone() {
        let mut guard = InsertGuard::default();
        // Not bypassed: what is being counted against it stays counted.
        for block in 0..MISS_WINDOW_BLOCKS - 1 {
            guard.note_bridge_block_at(true, AudioguardLevel::High, false);
            if block % 64 == 0 {
                guard.rearm_from_lane();
            }
        }
        guard.note_bridge_block_at(true, AudioguardLevel::High, false);
        assert!(guard.bypassed, "lane edges did not reset the miss window");
        assert_eq!(guard.health.trip(), InsertTrip::MissedDeadlines);

        guard.rearm_from_lane();
        assert!(!guard.bypassed, "the lane switched it back on");
        assert_eq!(guard.health.trip(), InsertTrip::None);
        guard.trip(InsertTrip::InvalidOutput);
        guard.rearm_from_lane();
        assert!(guard.bypassed, "a second time takes the user's switch");
        assert_eq!(guard.health.trip(), InsertTrip::InvalidOutput);

        guard.rearm();
        assert!(!guard.bypassed);
        guard.trip(InsertTrip::InvalidOutput);
        guard.rearm_from_lane();
        assert!(
            !guard.bypassed,
            "the user's switch gives the lane its turn back"
        );
    }

    /// The trip after a re-arm is reported again, whichever way the render's
    /// re-arm and the control thread's report interleaved. With two atomics a
    /// report that landed inside the re-arm left "reported, not tripped", and
    /// the next bypass was never shown in the project.
    #[test]
    fn a_trip_after_a_rearm_is_reported_again() {
        let mut guard = InsertGuard::default();
        let health = Arc::clone(&guard.health);
        guard.trip(InsertTrip::InvalidOutput);
        assert_eq!(health.take_unreported(), Some(InsertTrip::InvalidOutput));
        guard.rearm();
        guard.trip(InsertTrip::MissedDeadlines);
        assert_eq!(health.take_unreported(), Some(InsertTrip::MissedDeadlines));
        assert_eq!(health.take_unreported(), None);

        // A re-arm between the control side's read and its mark: the stale
        // report is dropped, and nothing is left marked.
        guard.trip(InsertTrip::InvalidOutput);
        let stale = health.state.load(Ordering::Relaxed);
        guard.rearm();
        assert!(health
            .state
            .compare_exchange(
                stale,
                stale | InsertHealth::REPORTED,
                Ordering::Relaxed,
                Ordering::Relaxed
            )
            .is_err());
        guard.trip(InsertTrip::InvalidOutput);
        assert_eq!(health.take_unreported(), Some(InsertTrip::InvalidOutput));
    }

    /// The ring and the callback's wait are sized from the stream's own rate,
    /// not the shared one, which is stale or zero until the stream is open.
    #[test]
    fn render_ahead_is_planned_from_the_streams_own_rate() {
        let shared = playing_shared();
        shared.sample_rate.store(0, Ordering::Relaxed);
        let (core, _tx) = playing_core();
        let inner = GuardInner {
            core: Mutex::new(core),
            ring: GuardRing::new(2),
            channels: 2,
            sample_rate: 48_000,
            shared: Arc::clone(&shared),
            block_frames: AtomicUsize::new(256),
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        };
        assert_eq!(inner.render_ahead_plan(false), Some((256, 768)));
        assert_eq!(inner.render_ahead_plan(true), None, "held off while played");
        inner
            .block_frames
            .store(MAX_GUARD_BLOCK_FRAMES + 1, Ordering::Relaxed);
        assert_eq!(inner.render_ahead_plan(false), None);
        assert_eq!(block_period(480, 48_000), Duration::from_millis(10));
    }

    /// A buffer that is not whole frames gets its ragged tail silenced.
    #[test]
    fn a_ragged_tail_is_silence() {
        let shared = playing_shared();
        let (core, _tx) = playing_core();
        let mut guarded = GuardedRenderer::new(core, 2, Arc::clone(&shared));
        let mut out = vec![9.0f32; 64 * 2 + 1];
        guarded.render(&mut out);
        assert_eq!(out[64 * 2], 0.0);
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
        assert_eq!(guard.health.trip(), InsertTrip::InvalidOutput);
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

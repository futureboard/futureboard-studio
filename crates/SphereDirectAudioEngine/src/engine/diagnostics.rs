//! Engine state, dropout and slow-block diagnostics, and the callback timing record.

use super::*;

// ── Audio engine lifecycle (control → audio, Relaxed reads in callback) ───────

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioEngineState {
    Running = 0,
    Paused = 1,
    LoadingProject = 2,
    ClosingProject = 3,
    DeviceSwitching = 4,
    Suspended = 5,
}

impl AudioEngineState {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Paused,
            2 => Self::LoadingProject,
            3 => Self::ClosingProject,
            4 => Self::DeviceSwitching,
            5 => Self::Suspended,
            _ => Self::Running,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::LoadingProject => "LoadingProject",
            Self::ClosingProject => "ClosingProject",
            Self::DeviceSwitching => "DeviceSwitching",
            Self::Suspended => "Suspended",
        }
    }

    /// Whether the realtime graph runs this block.
    ///
    /// This is a question about the **engine**, not about the transport, and
    /// the distinction is the whole point: a stopped transport still has to
    /// process live input, monitoring, MIDI preview, instruments, insert FX,
    /// sends, master FX, plugin tails and metering. Only the timeline stops.
    ///
    /// `Paused` therefore renders. The three states that do not are the ones
    /// where there is genuinely nothing to render into or from: the project is
    /// being torn down, the device is being swapped, or the engine is
    /// suspended.
    ///
    /// `LoadingProject` renders too. It means "the control thread is building
    /// the next graph"; the graph the callback is holding is untouched and
    /// still perfectly renderable until the swap arrives, and the swap itself
    /// preserves the transport (see the `LoadProject` arm in `backend::render`).
    pub fn renders_graph(self) -> bool {
        matches!(self, Self::Running | Self::LoadingProject | Self::Paused)
    }

    /// Whether this state means the engine cannot produce audio at all.
    ///
    /// The inverse of [`Self::renders_graph`]. Treating too much as silence is
    /// what made playback stop mid-song and what made a stopped transport kill
    /// preview, monitoring and live FX: every path that failed to send the
    /// waking command left the engine mute, so the only way back was pressing
    /// Play, whose `StartTransport` forces `Running`.
    pub fn outputs_silence(self) -> bool {
        !self.renders_graph()
    }

    /// Whether the **transport** may advance: the playhead moves and timeline
    /// clips are read. Strictly narrower than [`Self::renders_graph`], because
    /// `Paused` renders the graph without advancing the timeline.
    pub fn renders_transport(self) -> bool {
        matches!(self, Self::Running | Self::LoadingProject)
    }
}

/// Solfege Audioguard level (Settings → Playback). Sets render-ahead, the
/// output guard and how early a block approaching its deadline is flagged; see
/// [`crate::audioguard`]. `Normal` is the default.
pub use crate::audioguard::AudioguardLevel;
/// The level's former name, kept for callers that still use it.
pub type DropoutProtectionMode = AudioguardLevel;

/// Why the most recent dropout-risk block was flagged. Stored as a `u8` in
/// [`SharedState`] (audio → control).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropoutReason {
    None = 0,
    CallbackOverrun = 1,
    GraphSwapLate = 2,
    DiskCacheMiss = 3,
    PluginOverrun = 4,
    /// The device asked for a block the Audioguard ring did not have ready,
    /// while the guard thread held the graph.
    RenderAheadMiss = 5,
}

impl DropoutReason {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::CallbackOverrun,
            2 => Self::GraphSwapLate,
            3 => Self::DiskCacheMiss,
            4 => Self::PluginOverrun,
            5 => Self::RenderAheadMiss,
            _ => Self::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CallbackOverrun => "callback-overrun",
            Self::GraphSwapLate => "graph-swap-late",
            Self::DiskCacheMiss => "disk-cache-miss",
            Self::PluginOverrun => "plugin-overrun",
            Self::RenderAheadMiss => "render-ahead-miss",
        }
    }
}

/// Snapshot of dropout-protection diagnostics, read off the control thread.
#[derive(Clone, Copy, Debug)]
pub struct DropoutDiagnostics {
    pub protection_mode: DropoutProtectionMode,
    pub dropout_count: u64,
    pub last_reason: DropoutReason,
    pub callback_last_us: u32,
    pub callback_max_us: u32,
    pub callback_deadline_us: u32,
    pub slow_callback_count: u64,
}

/// Realtime-safe output-callback timing + dropout detection. Atomics only — no
/// allocation, lock, logging, or syscall — so it is safe to call at the end of
/// every audio callback. Shared by the legacy in-process callback and the DAUx
/// backend so both paths report identical diagnostics.
#[inline]
pub(crate) fn record_output_callback_timing(
    shared: &SharedState,
    elapsed_us: u32,
    block_frames: usize,
    sample_rate: u32,
    count_dropouts: bool,
) {
    shared.last_callback_us.store(elapsed_us, Ordering::Relaxed);
    if elapsed_us > shared.max_callback_us.load(Ordering::Relaxed) {
        shared.max_callback_us.store(elapsed_us, Ordering::Relaxed);
    }
    // Wall-clock budget for this block: it must render in less time than it takes
    // to play, or the device starves (xrun).
    let deadline_us = if sample_rate > 0 {
        ((block_frames as u64 * 1_000_000) / sample_rate as u64).min(u32::MAX as u64) as u32
    } else {
        0
    };
    shared
        .callback_deadline_us
        .store(deadline_us, Ordering::Relaxed);
    if elapsed_us >= 2_000 {
        shared.slow_callback_count.fetch_add(1, Ordering::Relaxed);
    }
    // A block rendered ahead by Audioguard has the ring's headroom, not one
    // period: its time is published for the load meter, and only a block the
    // device then misses counts as a dropout (`audioguard::GuardedRenderer`).
    if deadline_us > 0 && count_dropouts && elapsed_us >= deadline_us {
        shared.callback_overruns.fetch_add(1, Ordering::Relaxed);
    }
    if deadline_us > 0 && count_dropouts {
        let mode =
            DropoutProtectionMode::from_u8(shared.dropout_protection_mode.load(Ordering::Relaxed));
        let (num, den) = mode.dropout_threshold_ratio();
        let threshold = ((deadline_us as u64 * num) / den).min(u32::MAX as u64) as u32;
        if elapsed_us >= threshold {
            shared.dropout_count.fetch_add(1, Ordering::Relaxed);
            shared
                .dropout_last_reason
                .store(DropoutReason::CallbackOverrun as u8, Ordering::Relaxed);
        }
    }
}

/// One slow output block, as the callback saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlowBlock {
    pub elapsed_us: u32,
    pub deadline_us: u32,
    pub frames: u32,
    pub state: AudioEngineState,
}

impl SlowBlock {
    /// `error` when the block missed its deadline (the device starved),
    /// `warning` when it was over halfway there.
    pub fn severity(&self) -> &'static str {
        if self.deadline_us > 0 && self.elapsed_us >= self.deadline_us {
            "error"
        } else {
            "warning"
        }
    }

    pub fn load_pct(&self) -> u32 {
        if self.deadline_us > 0 {
            (self.elapsed_us as u64 * 100 / self.deadline_us as u64) as u32
        } else {
            0
        }
    }
}

/// Latest-value mailbox for the slow-block watchdog: the audio callback
/// publishes with three relaxed stores and a release increment, the control
/// thread takes what is new. The callback used to `eprintln!` the line itself
/// — formatting, a stderr lock and a write, in exactly the blocks that were
/// already close to their deadline.
///
/// Only the most recent report is kept: a sustained stall is one line per
/// drain, not a backlog. A report overwritten while the reader copies it can
/// come out mixed; it is a diagnostic line, never a control input.
#[derive(Debug, Default)]
pub struct SlowBlockReport {
    pub(super) seq: AtomicU64,
    pub(super) timing: AtomicU64,
    pub(super) detail: AtomicU64,
    /// Control side: the sequence last taken.
    pub(super) taken: AtomicU64,
}

impl SlowBlockReport {
    /// Audio thread. Atomics only.
    #[inline]
    pub fn publish(&self, block: SlowBlock) {
        self.timing.store(
            (u64::from(block.elapsed_us) << 32) | u64::from(block.deadline_us),
            Ordering::Relaxed,
        );
        self.detail.store(
            (u64::from(block.frames) << 8) | block.state as u64,
            Ordering::Relaxed,
        );
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// Control thread: the latest report if one arrived since the last take,
    /// with how many were published in between (including it).
    pub fn take(&self) -> Option<(SlowBlock, u64)> {
        let seq = self.seq.load(Ordering::Acquire);
        let taken = self.taken.swap(seq, Ordering::Relaxed);
        if seq == taken {
            return None;
        }
        let timing = self.timing.load(Ordering::Relaxed);
        let detail = self.detail.load(Ordering::Relaxed);
        Some((
            SlowBlock {
                elapsed_us: (timing >> 32) as u32,
                deadline_us: timing as u32,
                frames: (detail >> 8) as u32,
                state: AudioEngineState::from_u8(detail as u8),
            },
            seq.wrapping_sub(taken),
        ))
    }
}

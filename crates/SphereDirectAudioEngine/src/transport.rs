//! Transport clock snapshot and loop-wrap helpers.
//!
//! The audio callback owns the sample clock while running; the control thread
//! reads atomics to build a transport snapshot for UI polling.

use std::sync::atomic::Ordering;

use crate::engine::SharedState;
use crate::runtime::RuntimeProject;
use crate::tempo_map::TempoMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopBounds {
    pub start: u64,
    pub end: u64,
}

/// What the transport is doing, as one value.
///
/// Two independent booleans — `playing` and "a take is capturing" — can encode
/// `playing = false, capturing = true`, and that combination is precisely the
/// bug it used to produce: a Stop that moved the playhead flag but left the
/// writer consuming input, so the file kept growing and the next Record
/// continued the same take. One value cannot represent it.
///
/// The engine derives this from its own flags rather than storing it, so the
/// invariant holds by construction for every caller and every backend:
/// **capture only exists inside `Recording`, and `Recording` only exists while
/// the transport is running.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    /// The playhead does not advance and no take is consuming input. The
    /// realtime graph still runs — live input, monitoring, preview, FX and
    /// plugin tails are not the transport's business.
    Stopped,
    /// The playhead advances and the timeline is read.
    Playing,
    /// Playing, and a take is consuming input into a writer.
    Recording,
}

impl TransportState {
    /// Collapse the engine's two flags into the one legal state.
    ///
    /// Capture while stopped is not representable: it resolves to `Stopped`,
    /// which is the answer that matches what a stopped transport must do.
    pub fn from_flags(playing: bool, capturing: bool) -> Self {
        match (playing, capturing) {
            (false, _) => Self::Stopped,
            (true, false) => Self::Playing,
            (true, true) => Self::Recording,
        }
    }

    /// Whether the playhead advances and timeline clips are read.
    pub fn advances_timeline(self) -> bool {
        matches!(self, Self::Playing | Self::Recording)
    }

    /// Whether a take may consume input this block.
    pub fn captures_input(self) -> bool {
        matches!(self, Self::Recording)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Playing => "Playing",
            Self::Recording => "Recording",
        }
    }
}

/// Immutable transport state for UI polling and diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeTransportSnapshot {
    pub playing: bool,
    pub position_samples: u64,
    pub position_seconds: f64,
    pub position_beats: f64,
    pub loop_enabled: bool,
    pub loop_start_seconds: f64,
    pub loop_end_seconds: f64,
    pub bpm: f64,
    pub time_signature: [u32; 2],
    pub metronome_enabled: bool,
}

impl RuntimeTransportSnapshot {
    pub fn from_shared(shared: &SharedState, tempo_map: &TempoMap) -> Self {
        let sample_rate = shared.sample_rate.load(Ordering::Relaxed).max(1) as f64;
        let position_samples = shared.playback_position();
        let position_seconds = position_samples as f64 / sample_rate;
        let loop_start_samples = shared.loop_start_samples.load(Ordering::Relaxed);
        let loop_end_samples = shared.loop_end_samples.load(Ordering::Relaxed);
        let position_beats = tempo_map.beat_at_seconds(position_seconds);

        Self {
            playing: shared.playing.load(Ordering::Relaxed),
            position_samples,
            position_seconds,
            position_beats,
            loop_enabled: shared.loop_enabled.load(Ordering::Relaxed),
            loop_start_seconds: loop_start_samples as f64 / sample_rate,
            loop_end_seconds: loop_end_samples as f64 / sample_rate,
            bpm: tempo_map.bpm_at_beat(position_beats),
            time_signature: [
                shared.time_sig_num.load(Ordering::Relaxed).max(1),
                shared.time_sig_den.load(Ordering::Relaxed).max(1),
            ],
            metronome_enabled: shared.metronome_enabled.load(Ordering::Relaxed),
        }
    }
}

/// If the playhead crossed the loop end, wrap to loop start and return the
/// new sample position. Realtime-safe: atomics only.
pub fn apply_loop_wrap(
    shared: &SharedState,
    runtime: &mut RuntimeProject,
    sample_rate: u32,
    on_reposition: impl FnOnce(u64),
) {
    if !shared.loop_enabled.load(Ordering::Relaxed) {
        return;
    }
    let start = shared.loop_start_samples.load(Ordering::Relaxed);
    let end = shared.loop_end_samples.load(Ordering::Relaxed);
    if end <= start {
        return;
    }
    let pos = shared.position_samples.load(Ordering::Relaxed);
    if pos >= end {
        shared.position_samples.store(start, Ordering::Relaxed);
        on_reposition(start);
        runtime.reset_midi_playback(start);
        let _ = sample_rate;
    }
}

#[inline]
pub fn active_loop_bounds(shared: &SharedState) -> Option<LoopBounds> {
    if !shared.loop_enabled.load(Ordering::Relaxed) {
        return None;
    }
    let start = shared.loop_start_samples.load(Ordering::Relaxed);
    let end = shared.loop_end_samples.load(Ordering::Relaxed);
    (end > start).then_some(LoopBounds { start, end })
}

#[inline]
pub fn normalize_loop_position(position: u64, loop_bounds: Option<LoopBounds>) -> u64 {
    let Some(bounds) = loop_bounds else {
        return position;
    };
    if position >= bounds.end {
        bounds.start
    } else {
        position
    }
}

#[inline]
pub fn segment_frames_until_loop_wrap(
    position: u64,
    remaining_frames: u64,
    loop_bounds: Option<LoopBounds>,
) -> u64 {
    if remaining_frames == 0 {
        return 0;
    }
    let Some(bounds) = loop_bounds else {
        return remaining_frames;
    };
    if position >= bounds.end {
        return 1.min(remaining_frames);
    }
    let to_end = bounds.end.saturating_sub(position);
    if to_end == 0 {
        1.min(remaining_frames)
    } else {
        remaining_frames.min(to_end)
    }
}

#[inline]
pub fn advance_loop_position(
    mut position: u64,
    mut frames: u64,
    loop_bounds: Option<LoopBounds>,
) -> (u64, bool) {
    let Some(bounds) = loop_bounds else {
        return (position.saturating_add(frames), false);
    };
    let mut wrapped = false;
    if position >= bounds.end {
        position = bounds.start;
        wrapped = true;
    }
    while frames > 0 {
        let to_end = bounds.end.saturating_sub(position);
        if to_end == 0 {
            position = bounds.start;
            wrapped = true;
            continue;
        }
        if frames < to_end {
            position = position.saturating_add(frames);
            break;
        }
        frames -= to_end;
        position = bounds.start;
        wrapped = true;
    }
    (position, wrapped)
}

pub fn store_f64_bits(atomic: &std::sync::atomic::AtomicU64, value: f64) {
    atomic.store(value.to_bits(), Ordering::Relaxed);
}

pub fn f64_from_bits(bits: u64) -> f64 {
    f64::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo_map::TempoMap;

    #[test]
    fn snapshot_maps_seconds_to_beats() {
        let shared = SharedState::default();
        shared.sample_rate.store(48_000, Ordering::Relaxed);
        shared.position_samples.store(48_000, Ordering::Relaxed);
        store_f64_bits(&shared.bpm_bits, 120.0);

        let snap = RuntimeTransportSnapshot::from_shared(&shared, &TempoMap::static_tempo(120.0));
        assert!((snap.position_seconds - 1.0).abs() < 1e-9);
        assert!((snap.position_beats - 2.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod transport_state_tests {
    use super::TransportState;

    /// The invalid state the two booleans allowed. Whatever set "capturing"
    /// while the transport was stopped, the answer is Stopped — nothing
    /// downstream can read it as a licence to keep writing.
    #[test]
    fn a_stopped_transport_never_captures() {
        let state = TransportState::from_flags(false, true);
        assert_eq!(state, TransportState::Stopped);
        assert!(!state.captures_input());
        assert!(!state.advances_timeline());
    }

    #[test]
    fn recording_is_playing_that_also_captures() {
        let state = TransportState::from_flags(true, true);
        assert_eq!(state, TransportState::Recording);
        assert!(state.captures_input());
        assert!(state.advances_timeline());
    }

    #[test]
    fn playing_advances_without_capturing() {
        let state = TransportState::from_flags(true, false);
        assert_eq!(state, TransportState::Playing);
        assert!(!state.captures_input());
        assert!(state.advances_timeline());
    }

    /// Capture is a strict subset of advancing: if that ever inverts, a stopped
    /// transport is writing a file again.
    #[test]
    fn capture_implies_advancing() {
        for state in [
            TransportState::Stopped,
            TransportState::Playing,
            TransportState::Recording,
        ] {
            if state.captures_input() {
                assert!(state.advances_timeline(), "{state:?}");
            }
        }
    }
}

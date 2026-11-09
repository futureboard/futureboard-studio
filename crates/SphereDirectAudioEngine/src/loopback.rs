//! Track loopback: one track's output as another track's input.
//!
//! ```txt
//! Instrument track (VSTi) ──▶ post-fader block ──▶ Audio track input
//!                                                  monitor · meter · record
//! ```
//!
//! This is what lets an instrument be recorded as audio without a bounce: point
//! an audio track's Input at the instrument track, arm it, and the take is the
//! instrument's own output — through its inserts and its fader, the same signal
//! an export would capture.
//!
//! # Why it is a device id
//!
//! A loopback source is selected through Audio Connections like any other
//! input, under a device id prefix — exactly the way a remote Audio Jam stream
//! is (see [`crate::jam_bus::JAM_DEVICE_PREFIX`]). That keeps one routing model
//! rather than a second, private one: the same stable ids, the same
//! non-destructive behaviour when the source disappears, the same Input menu.
//!
//! # Timing
//!
//! The destination reads what the source rendered in the **previous** callback,
//! so a loopback costs exactly one buffer of latency and nothing else.
//!
//! That is deliberate, and it is what makes the feature safe. A track's input
//! is mixed before any track has rendered, so reading the source's *current*
//! block would read silence; ordering the render around it would need a
//! topological sort of the track graph and a rule for what to do about a cycle.
//! With a one-block delay there is no ordering constraint and no cycle to
//! resolve — routing A into B and B into A is simply a delay line, not a hang.
//! At 256 frames and 48 kHz that is 5.3 ms.

/// Device-id prefix that marks an Audio Connections port as another track's
/// output rather than a hardware input.
///
/// Deliberately not `jam:`: `jam_stream_id` strips that prefix to recover a
/// stream id, and a loopback source is a track id.
pub const LOOPBACK_DEVICE_PREFIX: &str = "trk:";

/// Build the Audio Connections device id for one source track.
pub fn loopback_device_id(track_id: &str) -> String {
    format!("{LOOPBACK_DEVICE_PREFIX}{track_id}")
}

/// The source track id inside a loopback device id, or `None` for anything
/// else.
pub fn loopback_track_id(device_id: &str) -> Option<&str> {
    device_id
        .strip_prefix(LOOPBACK_DEVICE_PREFIX)
        .filter(|id| !id.is_empty())
}

/// Whether a device id names a track loopback source.
pub fn is_loopback_device(device_id: &str) -> bool {
    loopback_track_id(device_id).is_some()
}

/// A lane with no destination in the current graph: it records silence.
const NO_TRACK: u32 = u32::MAX;

/// Frames the interleaving scratch holds. A longer callback is recorded in
/// chunks of this size, so the scratch never grows on the audio thread.
const SCRATCH_FRAMES: usize = 2048;

/// A take recording armed tracks whose input is another track.
///
/// ```txt
/// render callback ──▶ source post-fader block ──▶ lane k ──▶ RecordSink ──▶ disk writer
/// ```
///
/// There is no capture stream to tap: the signal is produced by the render
/// callback itself, so that is where it is recorded. That is also why a
/// loopback take needs no audio interface at all — only an output stream that
/// runs the graph.
///
/// Lane `k` records interleaved channels `2k` and `2k + 1` of the take, which
/// is what the take's track configs point at. Each lane names its
/// **destination** track; the source and channel mode come from that track's
/// resolved [`crate::runtime::RuntimeTrackInputSource::Loopback`] route, so
/// the recording is always exactly what the destination monitors.
///
/// Unlike monitoring, which reads the previous callback's block, the recorder
/// runs after the graph and takes the block the source rendered in *this*
/// callback, so the take carries no loopback delay.
///
/// # Realtime contract
///
/// The callback only reads atomics, copies into a scratch preallocated on the
/// control thread, and hands blocks to the take's bounded pipeline. The state
/// sits behind a mutex the control thread never takes while the recorder is
/// installed; the callback only ever `try_lock`s it, so it cannot wait.
pub struct LoopbackRecorder {
    /// Destination track index per lane, rewritten on the control thread for
    /// every graph it builds while the take runs.
    destinations: Box<[std::sync::atomic::AtomicU32]>,
    state: parking_lot::Mutex<LoopbackRecordState>,
}

struct LoopbackRecordState {
    sink: crate::capture_tap::RecordSink,
    /// Interleaved, `lanes × 2` channels wide, [`SCRATCH_FRAMES`] long.
    scratch: Vec<f32>,
}

impl std::fmt::Debug for LoopbackRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopbackRecorder")
            .field("lanes", &self.destinations.len())
            .finish_non_exhaustive()
    }
}

impl LoopbackRecorder {
    /// A recorder of `lanes` stereo lanes feeding `sink`. Allocates; control
    /// thread only.
    pub(crate) fn new(sink: crate::capture_tap::RecordSink, lanes: usize) -> Self {
        Self {
            destinations: (0..lanes)
                .map(|_| std::sync::atomic::AtomicU32::new(NO_TRACK))
                .collect(),
            state: parking_lot::Mutex::new(LoopbackRecordState {
                sink,
                scratch: vec![0.0; lanes.max(1) * 2 * SCRATCH_FRAMES],
            }),
        }
    }

    /// Interleaved channels a take recorded by `lanes` lanes carries.
    pub fn channels_for(lanes: usize) -> usize {
        lanes * 2
    }

    pub fn lanes(&self) -> usize {
        self.destinations.len()
    }

    /// Point lane `k` at `track_indices[k]`. A lane whose track is not in the
    /// graph records silence. Control thread.
    pub fn map_destinations(&self, track_indices: &[Option<usize>]) {
        use std::sync::atomic::Ordering;
        for (lane, slot) in self.destinations.iter().enumerate() {
            let index = track_indices
                .get(lane)
                .copied()
                .flatten()
                .and_then(|index| u32::try_from(index).ok())
                .unwrap_or(NO_TRACK);
            slot.store(index, Ordering::Release);
        }
    }

    /// Record one rendered block. Audio thread, after the graph has run.
    ///
    /// `base_sample` is the transport sample the block starts at.
    pub fn capture_block(
        &self,
        tracks: &[crate::runtime::RuntimeTrack],
        frames: usize,
        base_sample: u64,
        shared: &crate::engine::SharedState,
    ) {
        use std::sync::atomic::Ordering;
        // Never contended: the control thread does not take this lock while
        // the recorder is installed, and one graph renders one block at a time.
        let Some(mut guard) = self.state.try_lock() else {
            return;
        };
        let LoopbackRecordState { sink, scratch } = &mut *guard;
        let channels = Self::channels_for(self.destinations.len());
        if channels == 0 {
            return;
        }
        let chunk = scratch.len() / channels;
        let mut done = 0usize;
        while done < frames && chunk > 0 {
            let n = (frames - done).min(chunk);
            for (lane, destination) in self.destinations.iter().enumerate() {
                let source = tracks
                    .get(destination.load(Ordering::Acquire) as usize)
                    .and_then(|track| match track.input_source {
                        crate::runtime::RuntimeTrackInputSource::Loopback {
                            source_index,
                            mode,
                        } => Some((source_index as usize, mode)),
                        _ => None,
                    })
                    .and_then(|(index, mode)| {
                        tracks
                            .get(index)
                            .filter(|source| {
                                source.loopback_publish
                                    && source.loopback_out_l.len() >= done + n
                                    && source.loopback_out_r.len() >= done + n
                            })
                            .map(|source| (source, mode))
                    });
                for frame in 0..n {
                    let (l, r) = match source {
                        Some((source, mode)) => mode.apply(
                            source.loopback_out_l[done + frame],
                            source.loopback_out_r[done + frame],
                        ),
                        None => (0.0, 0.0),
                    };
                    let at = frame * channels + lane * 2;
                    scratch[at] = l;
                    scratch[at + 1] = r;
                }
            }
            let start = base_sample + done as u64;
            sink.capture(&scratch[..n * channels], channels, shared, || start);
            done += n;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_device_id_round_trips_to_its_track() {
        let id = loopback_device_id("trk_01J8");
        assert_eq!(id, "trk:trk_01J8");
        assert!(is_loopback_device(&id));
        assert_eq!(loopback_track_id(&id), Some("trk_01J8"));
    }

    /// The two virtual device families must never be mistaken for one another:
    /// a jam id resolves to a remote stream and a loopback id to a local track,
    /// and feeding one to the other's lookup would silently route the wrong
    /// audio.
    #[test]
    fn a_jam_stream_is_not_a_loopback_source_and_the_reverse() {
        let jam = crate::jam_bus::jam_device_id("str_1");
        assert!(!is_loopback_device(&jam));
        assert!(loopback_track_id(&jam).is_none());

        let loopback = loopback_device_id("trk_1");
        assert!(!crate::jam_bus::is_jam_device(&loopback));
        assert!(crate::jam_bus::jam_stream_id(&loopback).is_none());
    }

    #[test]
    fn hardware_and_malformed_ids_are_refused() {
        assert!(!is_loopback_device("focusrite"));
        assert!(!is_loopback_device("trk:"));
        assert!(loopback_track_id("trk").is_none());
    }
}

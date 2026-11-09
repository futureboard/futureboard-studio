//! Record fan-out for the persistent capture stream.
//!
//! # Why a tap
//!
//! Recording used to *be* a stream: pressing Record stopped the live input
//! stream and opened a new one, and Stop dropped that and re-opened the monitor
//! stream. Opening and closing a capture client mid-session is not free — it
//! renegotiates the endpoint, restarts a realtime thread and re-primes the
//! device buffer — and that is heard as a stutter at both ends of every take,
//! on the two moments a performer is least willing to hear one.
//!
//! So the stream stays. It is opened when a track is armed or monitored and
//! lives until the routing changes; a take is a *sink* installed into the
//! callback that is already running, and Stop removes it. Nothing in the device
//! path moves when recording starts or stops.
//!
//! This is the shape the ASIO session has always used
//! ([`crate::backend::asio_session::RecordSink`]); this module is the same
//! contract for the cpal backends, kept separate because the two callbacks own
//! different buffers and neither should have to grow a branch for the other.
//!
//! # Realtime contract
//!
//! The callback never allocates, locks, or deallocates:
//!
//! * commands arrive on a bounded channel drained at block start;
//! * a sink is carried boxed, so installing and retiring one moves a pointer;
//! * a retired sink leaves through a bounded trash channel, so both the
//!   `Sender` disconnect (which finalizes the disk writer) and the box's
//!   deallocation happen on the control thread;
//! * a full trash channel is not a reason to deallocate on the audio thread —
//!   the sink is held until there is room.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::engine::{f32_load, f32_store, SharedState};
use crate::input_ring::WaveformPeak;

/// Per-track preview bin accumulator.
///
/// One per armed track, on the channels *that track* records — not the global
/// monitor pair, or every track's growing waveform would be the same picture.
/// Allocated on the control thread and only mutated in place afterwards.
#[derive(Debug, Clone)]
pub struct PreviewAccum {
    pub l_ch: usize,
    pub r_ch: usize,
    min: f32,
    max: f32,
    sumsq: f32,
    count: usize,
}

impl PreviewAccum {
    pub fn new(l_ch: usize, r_ch: usize) -> Self {
        Self {
            l_ch,
            r_ch,
            min: f32::MAX,
            max: f32::MIN,
            sumsq: 0.0,
            count: 0,
        }
    }

    #[inline]
    fn reset(&mut self) {
        self.min = f32::MAX;
        self.max = f32::MIN;
        self.sumsq = 0.0;
        self.count = 0;
    }

    /// Fold one mono sample in, returning a finished bin when it completes.
    #[inline]
    fn push(&mut self, sample: f32, samples_per_bin: usize) -> Option<WaveformPeak> {
        self.min = self.min.min(sample);
        self.max = self.max.max(sample);
        self.sumsq += sample * sample;
        self.count += 1;
        if self.count < samples_per_bin {
            return None;
        }
        let peak = WaveformPeak {
            min: self.min,
            max: self.max,
            rms: (self.sumsq / self.count as f32).sqrt(),
        };
        self.reset();
        Some(peak)
    }
}

/// Everything the capture callback needs to feed one take.
///
/// Built by [`crate::recording`] on the control thread, carried in boxed, and
/// handed back through the trash channel when the take ends.
pub struct RecordSink {
    pub audio_tx: Sender<crate::recording::RecordBlock>,
    pub free_rx: Receiver<Vec<i32>>,
    pub free_tx: Sender<Vec<i32>>,
    /// Preview bin width in frames, from the take's sample rate.
    pub samples_per_bin: usize,
    /// When set, samples are only captured while the transport is rolling.
    pub capture_on_transport: bool,
    /// Blocks lost to an exhausted pool or a full queue. Shared with the
    /// session so the end-of-take report can say so.
    pub dropped_blocks: Arc<AtomicU64>,
    /// Cleared by `stop_recording` before the sink is detached, so the last
    /// blocks of a finishing take are not written after finalization began.
    pub active: Arc<AtomicBool>,
    /// Transport position of the first block this take actually captured, or
    /// [`u64::MAX`] while it has captured none.
    ///
    /// A take is armed before it captures — always by a block or two, and by a
    /// whole count-in when there is one — so the position read at arm time is
    /// not where the audio begins. Recording that arrival is what lets the
    /// finished take be placed on the sample it really started on instead of
    /// the sample it was asked to start on.
    pub first_capture_sample: Arc<AtomicU64>,
    /// The global preview mix, on the monitored pair.
    pub monitor: PreviewAccum,
    /// One per armed track, in `shared.recording_preview_track_ids` order.
    pub tracks: Vec<PreviewAccum>,
    /// Input frames lost since the last block reached the writer; sent with
    /// the next one (see [`crate::recording::send_capture_block`]).
    pub gap_frames: u64,
}

/// Control-thread handle: install a take's sink, remove it, reclaim it.
pub struct CaptureTapControl {
    cmd_tx: Sender<TapCommand>,
    trash_rx: Receiver<Box<RecordSink>>,
}

enum TapCommand {
    Set(Box<RecordSink>),
    Clear,
}

impl CaptureTapControl {
    /// Hand a take's sink to the running callback.
    ///
    /// Fails only when the command queue is full, which means the callback has
    /// stopped draining — the stream is gone. The caller falls back to opening
    /// its own capture stream rather than starting a take that captures nothing.
    // The error hands the caller its sink back to reuse; boxing it would
    // allocate on this path only to unbox it again.
    #[allow(clippy::result_large_err)]
    pub fn install(&self, sink: RecordSink) -> Result<(), RecordSink> {
        self.cmd_tx
            .try_send(TapCommand::Set(Box::new(sink)))
            .map_err(|error| match error.into_inner() {
                TapCommand::Set(sink) => *sink,
                TapCommand::Clear => unreachable!("sent a Set"),
            })
    }

    /// Detach the current sink. The callback releases its senders on the next
    /// block, which disconnects the disk writer and finalizes the take.
    pub fn clear(&self) {
        let _ = self.cmd_tx.try_send(TapCommand::Clear);
    }

    /// Drop every retired sink on this thread. Called after a take is
    /// finalized; the audio thread never deallocates one.
    pub fn drain_trash(&self) {
        while self.trash_rx.try_recv().is_ok() {}
    }
}

/// Audio-thread side: drained and driven from inside the capture callback.
pub struct CaptureTapCallback {
    cmd_rx: Receiver<TapCommand>,
    trash_tx: Sender<Box<RecordSink>>,
    active: Option<Box<RecordSink>>,
    /// A sink that has been replaced but could not be handed back yet because
    /// the trash channel was full. Held rather than dropped — dropping is a
    /// deallocation, and this is the audio thread.
    retiring: Option<Box<RecordSink>>,
}

/// Build a tap: the control handle stays with the stream's owner, the callback
/// half moves into the capture closure.
pub fn capture_tap() -> (CaptureTapControl, CaptureTapCallback) {
    let (cmd_tx, cmd_rx) = bounded::<TapCommand>(8);
    let (trash_tx, trash_rx) = bounded::<Box<RecordSink>>(8);
    (
        CaptureTapControl { cmd_tx, trash_rx },
        CaptureTapCallback {
            cmd_rx,
            trash_tx,
            active: None,
            retiring: None,
        },
    )
}

impl CaptureTapCallback {
    /// Whether a take is currently attached. Cheap enough to check per block.
    #[inline]
    pub fn has_sink(&self) -> bool {
        self.active.is_some()
    }

    /// Apply pending commands and try again to hand back anything retired.
    /// Call once at block start, before [`Self::capture_block`].
    #[inline]
    pub fn drain_commands(&mut self) {
        self.flush_retiring();
        while let Ok(command) = self.cmd_rx.try_recv() {
            let replaced = match command {
                TapCommand::Set(sink) => self.active.replace(sink),
                TapCommand::Clear => self.active.take(),
            };
            if let Some(sink) = replaced {
                self.retire(sink);
            }
        }
    }

    #[inline]
    fn retire(&mut self, sink: Box<RecordSink>) {
        match self.trash_tx.try_send(sink) {
            Ok(()) => {}
            Err(error) => {
                // Hold it. `flush_retiring` will try again next block, and the
                // one already held (if any) is the older of the two — keep the
                // newer, since the older's writer has long since finalized.
                self.retiring = Some(error.into_inner());
            }
        }
    }

    #[inline]
    fn flush_retiring(&mut self) {
        if let Some(sink) = self.retiring.take() {
            self.retire(sink);
        }
    }

    /// Fan one interleaved capture block out to the attached take.
    ///
    /// `data` is the raw device block and `channels` its interleaved channel
    /// count — the same geometry `build_track_writers` validated the take's
    /// channel selection against.
    ///
    /// Does nothing at all when no take is attached, which is the state the
    /// stream spends almost all of its life in.
    pub fn capture_block(&mut self, data: &[f32], channels: usize, shared: &SharedState) {
        let Some(sink) = self.active.as_mut() else {
            return;
        };
        sink.capture(data, channels, shared, || shared.render_position_now());
    }
}

impl RecordSink {
    /// Fan one interleaved block out to this take: preview bins, the record
    /// peak, and the disk writer.
    ///
    /// `position` is the transport sample the block starts at. It is asked for
    /// only on the take's first captured block, which stamps where the audio
    /// really begins. A capture stream passes the render position it is
    /// running against; the render callback's loopback tap passes the block it
    /// just rendered, which is exact.
    ///
    /// Realtime-safe: bounded `try_send`/`try_recv` and arithmetic, nothing
    /// else.
    pub(crate) fn capture(
        &mut self,
        data: &[f32],
        channels: usize,
        shared: &SharedState,
        position: impl FnOnce() -> u64,
    ) {
        let sink = self;
        let channels = channels.max(1);
        let capturing = sink.active.load(Ordering::Relaxed)
            && (!sink.capture_on_transport || shared.playing.load(Ordering::Relaxed));
        if !capturing {
            // Between arming and the downbeat there is nothing to record, and a
            // half-full bin from before the take must not be published as its
            // first peak.
            sink.monitor.reset();
            for track in &mut sink.tracks {
                track.reset();
            }
            return;
        }

        // The first block that is really captured stamps the take's true start.
        if sink.first_capture_sample.load(Ordering::Relaxed) == u64::MAX {
            let _ = sink.first_capture_sample.compare_exchange(
                u64::MAX,
                position(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }

        let samples_per_bin = sink.samples_per_bin.max(1);
        let mut record_peak = 0.0f32;
        for frame in data.chunks(channels) {
            let l = frame
                .get(sink.monitor.l_ch)
                .copied()
                .unwrap_or_else(|| frame.first().copied().unwrap_or(0.0))
                .clamp(-1.0, 1.0);
            let r = frame
                .get(sink.monitor.r_ch)
                .copied()
                .unwrap_or(l)
                .clamp(-1.0, 1.0);
            if let Some(peak) = sink.monitor.push((l + r) * 0.5, samples_per_bin) {
                shared.preview_ring.push(peak);
            }
            for (slot, track) in sink.tracks.iter_mut().enumerate() {
                let tl = frame
                    .get(track.l_ch)
                    .copied()
                    .unwrap_or(0.0)
                    .clamp(-1.0, 1.0);
                let tr = frame
                    .get(track.r_ch)
                    .copied()
                    .unwrap_or(tl)
                    .clamp(-1.0, 1.0);
                if let Some(peak) = track.push((tl + tr) * 0.5, samples_per_bin) {
                    shared.preview_rings[slot].push(peak);
                }
            }
            for sample in frame {
                record_peak = record_peak.max(sample.abs());
            }
        }

        let decayed = f32_load(shared.record_peak.load(Ordering::Relaxed)) * 0.9;
        shared
            .record_peak
            .store(f32_store(decayed.max(record_peak)), Ordering::Relaxed);

        // Hand the block to the disk writer. Every failure path returns the
        // block to the pool, counts a drop and keeps the lost length for the
        // writer to fill with silence; none of them blocks or allocates.
        if !crate::recording::send_capture_block(
            data,
            channels,
            crate::recording::f32_to_s32,
            &sink.audio_tx,
            &sink.free_rx,
            &sink.free_tx,
            &mut sink.gap_frames,
        ) {
            count_drop(sink, shared);
        }
    }
}

#[inline]
fn count_drop(sink: &RecordSink, shared: &SharedState) {
    sink.dropped_blocks.fetch_add(1, Ordering::Relaxed);
    shared.record_ring_overruns.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::recording::RecordBlock;

    fn sink(active: Arc<AtomicBool>) -> (RecordSink, Receiver<RecordBlock>) {
        let (audio_tx, audio_rx) = bounded::<RecordBlock>(4);
        let (free_tx, free_rx) = bounded::<Vec<i32>>(4);
        for _ in 0..4 {
            let _ = free_tx.try_send(Vec::with_capacity(1024));
        }
        (
            RecordSink {
                audio_tx,
                free_rx,
                free_tx,
                samples_per_bin: 1,
                capture_on_transport: false,
                dropped_blocks: Arc::new(AtomicU64::new(0)),
                active,
                first_capture_sample: Arc::new(AtomicU64::new(u64::MAX)),
                monitor: PreviewAccum::new(0, 1),
                tracks: vec![PreviewAccum::new(0, 1)],
                gap_frames: 0,
            },
            audio_rx,
        )
    }

    /// The stream runs for the whole session; a take is attached and detached
    /// under it. Nothing may be captured before the sink arrives or after it
    /// leaves — that is the entire point of the tap.
    #[test]
    fn a_block_is_only_captured_while_a_sink_is_attached() {
        let shared = Arc::new(SharedState::default());
        let (control, mut callback) = capture_tap();
        let block = [0.5f32, -0.5, 0.25, -0.25];

        callback.drain_commands();
        callback.capture_block(&block, 2, &shared);
        assert!(!callback.has_sink());

        let active = Arc::new(AtomicBool::new(true));
        let (sink, audio_rx) = sink(Arc::clone(&active));
        control.install(sink).map_err(|_| ()).expect("install");
        callback.drain_commands();
        assert!(callback.has_sink());
        callback.capture_block(&block, 2, &shared);
        assert_eq!(
            audio_rx.try_recv().expect("captured block").samples.len(),
            4
        );

        control.clear();
        callback.drain_commands();
        assert!(!callback.has_sink());
        callback.capture_block(&block, 2, &shared);
        assert!(audio_rx.try_recv().is_err(), "captured after detach");
    }

    /// A detached sink must reach the control thread, because dropping it is
    /// what disconnects the disk writer and finalizes the take — and the audio
    /// thread may not be the one to do it.
    #[test]
    fn a_detached_sink_is_handed_back_for_disposal() {
        let (control, mut callback) = capture_tap();
        let (sink, _audio_rx) = sink(Arc::new(AtomicBool::new(true)));
        control.install(sink).map_err(|_| ()).expect("install");
        callback.drain_commands();
        control.clear();
        callback.drain_commands();
        assert!(
            control.trash_rx.try_recv().is_ok(),
            "the retired sink never reached the control thread"
        );
    }

    /// Clearing the session's active flag stops capture immediately, without
    /// waiting for the detach command to reach the callback. `stop_recording`
    /// relies on it: the flag is cleared first, the sink removed after.
    #[test]
    fn clearing_the_active_flag_stops_capture_before_the_sink_is_detached() {
        let shared = Arc::new(SharedState::default());
        let (control, mut callback) = capture_tap();
        let active = Arc::new(AtomicBool::new(true));
        let (sink, audio_rx) = sink(Arc::clone(&active));
        control.install(sink).map_err(|_| ()).expect("install");
        callback.drain_commands();

        active.store(false, Ordering::Relaxed);
        callback.capture_block(&[0.5, -0.5], 2, &shared);
        assert!(audio_rx.try_recv().is_err(), "captured after the stop flag");
    }

    /// A take gated on the transport captures nothing while it is stopped, so a
    /// count-in can arm the writer before the downbeat without recording it.
    #[test]
    fn a_transport_gated_take_waits_for_the_transport() {
        let shared = Arc::new(SharedState::default());
        let (control, mut callback) = capture_tap();
        let (mut sink, audio_rx) = sink(Arc::new(AtomicBool::new(true)));
        sink.capture_on_transport = true;
        control.install(sink).map_err(|_| ()).expect("install");
        callback.drain_commands();

        shared.playing.store(false, Ordering::Relaxed);
        callback.capture_block(&[0.5, -0.5], 2, &shared);
        assert!(audio_rx.try_recv().is_err(), "captured while stopped");

        shared.playing.store(true, Ordering::Relaxed);
        callback.capture_block(&[0.5, -0.5], 2, &shared);
        assert!(audio_rx.try_recv().is_ok(), "did not capture while rolling");
    }

    /// Where the audio *starts* is the first block that was really captured,
    /// not the position the take was armed at. A count-in arms bars early; even
    /// without one, arming precedes the first block. Placing a take on its
    /// armed position puts the audio that far early, and two takes recorded the
    /// same way then refuse to line up with each other.
    #[test]
    fn the_take_stamps_the_position_of_its_first_captured_block() {
        let shared = Arc::new(SharedState::default());
        let (control, mut callback) = capture_tap();
        let (mut sink, _audio_rx) = sink(Arc::new(AtomicBool::new(true)));
        sink.capture_on_transport = true;
        let stamp = Arc::clone(&sink.first_capture_sample);
        control.install(sink).map_err(|_| ()).expect("install");
        callback.drain_commands();

        // Armed and waiting: the transport has not rolled, so nothing is
        // stamped however long the count-in runs.
        shared.position_samples.store(48_000, Ordering::Relaxed);
        shared.playing.store(false, Ordering::Relaxed);
        callback.capture_block(&[0.5, -0.5], 2, &shared);
        assert_eq!(stamp.load(Ordering::Relaxed), u64::MAX);

        // The downbeat.
        shared.position_samples.store(96_000, Ordering::Relaxed);
        shared.playing.store(true, Ordering::Relaxed);
        callback.capture_block(&[0.5, -0.5], 2, &shared);
        assert_eq!(stamp.load(Ordering::Relaxed), 96_000);

        // And it is the *first* block, not the latest one.
        shared.position_samples.store(96_512, Ordering::Relaxed);
        callback.capture_block(&[0.5, -0.5], 2, &shared);
        assert_eq!(stamp.load(Ordering::Relaxed), 96_000);
    }
}

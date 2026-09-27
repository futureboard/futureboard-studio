//! Realtime render capture: the live engine's output, recorded while the
//! project plays.
//!
//! A realtime render plays the arrangement through the running engine — the
//! path every plug-in, hardware insert and external instrument actually
//! sounds through — and records it, rather than rendering it offline. This is
//! the ring between the audio callback that produces those blocks and the
//! writer thread that encodes them.
//!
//! Lane 0 is the master mix (the graph's output after the master fader, before
//! click, test tone, audition or Control Room — what an offline mixdown
//! writes). Lanes 1.. are mixer channels, tapped at the same post-fader point
//! offline stems are. Every lane is stereo.
//!
//! Realtime rules: the callback side only stores into atomics that were
//! allocated when the capture was built, and never waits. A block the writer
//! has no room for is dropped and counted rather than blocking the callback;
//! so is a jump in the transport position. Either way the render is reported
//! incomplete instead of writing a file with a hole in it.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Highest track index a capture can map. Mixer channels beyond it are not
/// captured (and never exist in practice).
pub const MAX_CAPTURE_TRACK_INDEX: usize = 4096;
const NO_LANE: u32 = u32::MAX;
const NO_SAMPLE: u64 = u64::MAX;

pub struct RenderCapture {
    lanes: usize,
    capacity: usize,
    sample_rate: u32,
    /// `lanes × capacity × 2` samples as `f32` bits, lane-major: lane `l`
    /// frame `f` channel `c` is at `(l * capacity + f) * 2 + c`.
    samples: Box<[AtomicU32]>,
    /// Track index → lane, or [`NO_LANE`]. Written by the control thread.
    lane_of_track: Box<[AtomicU32]>,
    /// Frames committed by the callback / consumed by the writer. Only ever
    /// grow; the ring position is the value modulo `capacity`.
    written: AtomicU64,
    read: AtomicU64,
    /// Blocks are recorded only while this is set.
    recording: AtomicBool,
    /// The current block has room and is being staged.
    block_open: AtomicBool,
    block_frames: AtomicU64,
    /// Transport sample of the first recorded frame.
    first_sample: AtomicU64,
    /// Transport sample the next block must start at to be contiguous.
    expected_sample: AtomicU64,
    dropped_frames: AtomicU64,
    discontinuities: AtomicU64,
}

impl std::fmt::Debug for RenderCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderCapture")
            .field("lanes", &self.lanes)
            .field("capacity", &self.capacity)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl RenderCapture {
    /// A capture of the master plus `track_lanes` channels, holding up to
    /// `capacity` frames the writer has not taken yet. Allocates; control
    /// thread only.
    pub fn new(track_lanes: usize, capacity: usize, sample_rate: u32) -> Self {
        let lanes = track_lanes + 1;
        let capacity = capacity.max(1);
        Self {
            lanes,
            capacity,
            sample_rate,
            samples: (0..lanes * capacity * 2)
                .map(|_| AtomicU32::new(0))
                .collect(),
            lane_of_track: (0..MAX_CAPTURE_TRACK_INDEX)
                .map(|_| AtomicU32::new(NO_LANE))
                .collect(),
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
            recording: AtomicBool::new(false),
            block_open: AtomicBool::new(false),
            block_frames: AtomicU64::new(0),
            first_sample: AtomicU64::new(NO_SAMPLE),
            expected_sample: AtomicU64::new(NO_SAMPLE),
            dropped_frames: AtomicU64::new(0),
            discontinuities: AtomicU64::new(0),
        }
    }

    pub fn lanes(&self) -> usize {
        self.lanes
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Map the graph's track indices to lanes: `track_indices[k]` fills lane
    /// `k + 1`. Replaces the whole map. Control thread; the callback reads it
    /// per block, so a graph rebuilt with tracks reordered takes its new map
    /// from the block after this.
    pub fn map_tracks(&self, track_indices: &[Option<usize>]) {
        for lane in self.lane_of_track.iter() {
            lane.store(NO_LANE, Ordering::Relaxed);
        }
        for (k, index) in track_indices.iter().enumerate() {
            if let Some(index) = index.filter(|index| *index < MAX_CAPTURE_TRACK_INDEX) {
                self.lane_of_track[index].store((k + 1) as u32, Ordering::Release);
            }
        }
    }

    /// Start or stop recording blocks. Control thread.
    pub fn set_recording(&self, recording: bool) {
        self.recording.store(recording, Ordering::Release);
    }

    // ── Callback side ───────────────────────────────────────────────────────

    /// Open a block of `frames` starting at transport sample `base_sample`.
    /// `false` when nothing is being recorded or the writer has no room, in
    /// which case the block's stage calls do nothing. Every lane of the block
    /// starts silent, so a channel the graph skipped this block records
    /// silence, not a lap-old block.
    pub fn begin_block(&self, base_sample: u64, frames: usize) -> bool {
        self.block_open.store(false, Ordering::Relaxed);
        if frames == 0 || !self.recording.load(Ordering::Acquire) {
            return false;
        }
        let written = self.written.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        if written.saturating_sub(read) + frames as u64 > self.capacity as u64 {
            self.dropped_frames
                .fetch_add(frames as u64, Ordering::Relaxed);
            return false;
        }
        let expected = self.expected_sample.load(Ordering::Relaxed);
        if expected != NO_SAMPLE && expected != base_sample {
            self.discontinuities.fetch_add(1, Ordering::Relaxed);
        }
        if self.first_sample.load(Ordering::Relaxed) == NO_SAMPLE {
            self.first_sample.store(base_sample, Ordering::Release);
        }
        self.expected_sample
            .store(base_sample + frames as u64, Ordering::Relaxed);
        for lane in 0..self.lanes {
            for frame in 0..frames {
                let at = self.index(lane, written + frame as u64);
                self.samples[at].store(0, Ordering::Relaxed);
                self.samples[at + 1].store(0, Ordering::Relaxed);
            }
        }
        self.block_frames.store(frames as u64, Ordering::Relaxed);
        self.block_open.store(true, Ordering::Relaxed);
        true
    }

    /// Stage the post-fader block of the track at `track_index`, if it has a
    /// lane.
    #[inline]
    pub fn stage_track(&self, track_index: usize, left: &[f32], right: &[f32], frames: usize) {
        if !self.block_open.load(Ordering::Relaxed) {
            return;
        }
        let Some(lane) = self
            .lane_of_track
            .get(track_index)
            .map(|lane| lane.load(Ordering::Acquire))
            .filter(|lane| *lane != NO_LANE)
        else {
            return;
        };
        let frames = frames
            .min(self.block_frames.load(Ordering::Relaxed) as usize)
            .min(left.len())
            .min(right.len());
        let written = self.written.load(Ordering::Relaxed);
        for frame in 0..frames {
            let at = self.index(lane as usize, written + frame as u64);
            self.samples[at].store(left[frame].to_bits(), Ordering::Relaxed);
            self.samples[at + 1].store(right[frame].to_bits(), Ordering::Relaxed);
        }
    }

    /// Stage the master mix from the device buffer: `channels`-wide
    /// interleaved, the mix on the first two.
    #[inline]
    pub fn stage_master_interleaved(&self, output: &[f32], channels: usize, frames: usize) {
        if !self.block_open.load(Ordering::Relaxed) || channels == 0 {
            return;
        }
        let frames = frames
            .min(self.block_frames.load(Ordering::Relaxed) as usize)
            .min(output.len() / channels);
        let written = self.written.load(Ordering::Relaxed);
        for frame in 0..frames {
            let left = output[frame * channels];
            let right = if channels > 1 {
                output[frame * channels + 1]
            } else {
                left
            };
            let at = self.index(0, written + frame as u64);
            self.samples[at].store(left.to_bits(), Ordering::Relaxed);
            self.samples[at + 1].store(right.to_bits(), Ordering::Relaxed);
        }
    }

    /// Publish the staged block to the writer.
    #[inline]
    pub fn commit_block(&self) {
        if !self.block_open.swap(false, Ordering::Relaxed) {
            return;
        }
        let frames = self.block_frames.load(Ordering::Relaxed);
        self.written.fetch_add(frames, Ordering::Release);
    }

    #[inline]
    fn index(&self, lane: usize, frame: u64) -> usize {
        (lane * self.capacity + (frame % self.capacity as u64) as usize) * 2
    }

    // ── Writer side ─────────────────────────────────────────────────────────

    /// Take up to `max_frames` committed frames into `lanes` (one stereo
    /// interleaved buffer per lane, cleared first). Returns the frames taken.
    pub fn read(&self, lanes: &mut [Vec<f32>], max_frames: usize) -> usize {
        let written = self.written.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Relaxed);
        let frames = (written.saturating_sub(read) as usize).min(max_frames);
        for (lane, out) in lanes.iter_mut().enumerate().take(self.lanes) {
            out.clear();
            out.reserve(frames * 2);
            for frame in 0..frames {
                let at = self.index(lane, read + frame as u64);
                out.push(f32::from_bits(self.samples[at].load(Ordering::Relaxed)));
                out.push(f32::from_bits(self.samples[at + 1].load(Ordering::Relaxed)));
            }
        }
        self.read.fetch_add(frames as u64, Ordering::Release);
        frames
    }

    /// Transport sample of the first recorded frame, once one is.
    pub fn first_sample(&self) -> Option<u64> {
        let sample = self.first_sample.load(Ordering::Acquire);
        (sample != NO_SAMPLE).then_some(sample)
    }

    /// Frames lost because the writer fell a whole ring behind.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// Times the transport did not continue where the last block ended (a
    /// seek, a loop wrap, a stop and restart).
    pub fn discontinuities(&self) -> u64 {
        self.discontinuities.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(capture: &RenderCapture, base: u64, value: f32, frames: usize) -> bool {
        if !capture.begin_block(base, frames) {
            return false;
        }
        let track = vec![value; frames];
        capture.stage_track(3, &track, &track, frames);
        let master: Vec<f32> = (0..frames * 4).map(|_| -value).collect();
        capture.stage_master_interleaved(&master, 4, frames);
        capture.commit_block();
        true
    }

    #[test]
    fn blocks_reach_the_writer_in_their_lanes() {
        let capture = RenderCapture::new(2, 64, 48_000);
        capture.map_tracks(&[Some(3), Some(9)]);
        assert!(!block(&capture, 0, 0.5, 16), "not recording yet");
        capture.set_recording(true);
        assert!(block(&capture, 100, 0.5, 16));
        assert!(block(&capture, 116, 0.25, 16));
        let mut lanes = vec![Vec::new(); 3];
        assert_eq!(capture.read(&mut lanes, 1000), 32);
        assert_eq!(lanes[0][0], -0.5, "master");
        assert_eq!(lanes[1][0], 0.5, "track 3 is lane 1");
        assert_eq!(lanes[1][32], 0.25);
        assert!(
            lanes[2].iter().all(|s| *s == 0.0),
            "track 9 never played: silence"
        );
        assert_eq!(capture.first_sample(), Some(100));
        assert_eq!(capture.discontinuities(), 0);
    }

    #[test]
    fn a_full_ring_drops_and_counts_instead_of_waiting() {
        let capture = RenderCapture::new(0, 32, 48_000);
        capture.set_recording(true);
        assert!(block(&capture, 0, 0.1, 16));
        assert!(block(&capture, 16, 0.1, 16));
        assert!(!block(&capture, 32, 0.1, 16), "no room");
        assert_eq!(capture.dropped_frames(), 16);
        let mut lanes = vec![Vec::new()];
        assert_eq!(capture.read(&mut lanes, 1000), 32);
        assert!(block(&capture, 48, 0.1, 16));
        assert_eq!(capture.discontinuities(), 1, "the dropped block left a gap");
    }
}

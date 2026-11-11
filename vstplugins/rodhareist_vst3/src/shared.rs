//! What the audio thread, the editor and the background task thread share.
//! The audio thread only ever touches the atomics here.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use nih_plug::prelude::AtomicF32;
use rodharerist::{IrLoader, NamLoader, StageKind, PATH_SLOTS};

pub const TELEMETRY_SLOTS: usize = rodharerist::telemetry::SLOTS;

/// One block's input and output levels, as the DSP's meter frame has them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterReading {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub in_clip: bool,
    pub out_clip: bool,
}

/// The control-thread handles into one running `Dsp`. Replaced whenever the
/// plug-in builds a new `Dsp` (a sample-rate change).
pub struct ControlSide {
    pub nam: NamLoader,
    pub ir: IrLoader,
    pub sample_rate: f64,
    /// What this `Dsp` runs (or has queued), so a restore only reloads what
    /// changed.
    pub nam_loaded: Option<String>,
    pub ir_loaded: Option<String>,
    /// Bumped when a newer load or unload supersedes one still building.
    /// A finished build submits only if it still owns the generation.
    pub nam_generation: u64,
    pub ir_generation: u64,
    /// A file picked in the editor that has not finished building. A host
    /// re-activation leaves it running, or builds it again for a new `Dsp`;
    /// a state load replaces it.
    pub nam_pick: Option<Pick>,
    pub ir_pick: Option<Pick>,
}

impl ControlSide {
    /// The handles of a freshly built `Dsp`, with nothing loaded yet.
    pub fn new(nam: NamLoader, ir: IrLoader, sample_rate: f64) -> Self {
        Self {
            nam,
            ir,
            sample_rate,
            nam_loaded: None,
            ir_loaded: None,
            nam_generation: 0,
            ir_generation: 0,
            nam_pick: None,
            ir_pick: None,
        }
    }
}

/// What an editor pick handed over: the file's contents, kept until the
/// build lands so a new `Dsp` can be given the same file.
#[derive(Clone)]
pub enum Picked {
    Capture(Arc<str>),
    Impulse(Arc<[u8]>),
}

/// An editor pick still building.
pub struct Pick {
    pub name: String,
    pub data: Picked,
    /// The file name's state revision when it was picked. A state loaded
    /// since then is newer than the pick.
    pub revision: u64,
    /// The generation and control epoch its build runs under.
    pub generation: u64,
    pub epoch: u64,
}

pub type PathOrder = [Option<StageKind>; PATH_SLOTS];

/// A path packed five bits a slot (stage index + 1, 0 for empty), twelve
/// slots a word. Sixteen slots of sixteen stages no longer fit one word.
const SLOT_BITS: usize = 5;
const SLOT_MASK: u64 = (1 << SLOT_BITS) - 1;
const SLOTS_PER_WORD: usize = 64 / SLOT_BITS;
const PATH_WORDS: usize = PATH_SLOTS.div_ceil(SLOTS_PER_WORD);
const _: () = assert!(StageKind::COUNT < (1 << SLOT_BITS));

type PackedPath = [u64; PATH_WORDS];

fn pack_path(order: &PathOrder) -> PackedPath {
    let mut words = [0; PATH_WORDS];
    for (slot, stage) in order.iter().enumerate() {
        let value = stage.map_or(0, |stage| u64::from(stage.index()) + 1);
        words[slot / SLOTS_PER_WORD] |= value << ((slot % SLOTS_PER_WORD) * SLOT_BITS);
    }
    words
}

fn unpack_path(words: &PackedPath) -> PathOrder {
    std::array::from_fn(|slot| {
        let value =
            (words[slot / SLOTS_PER_WORD] >> ((slot % SLOTS_PER_WORD) * SLOT_BITS)) & SLOT_MASK;
        value
            .checked_sub(1)
            .and_then(|index| StageKind::from_index(index as i32))
    })
}

/// `PathTarget::current` when no chain edit is in flight.
const NO_PATH_TARGET: u64 = u64::MAX;

/// The order a chain edit from the editor is heading to. More than one word,
/// so it is double-buffered: the editor (the only writer) fills the copy not
/// in use and then points `current` at it, and a reader checks `current`
/// again after reading a copy. Lock-free on both sides; the audio thread never
/// waits for the editor.
struct PathTarget {
    copies: [[AtomicU64; PATH_WORDS]; 2],
    /// `NO_PATH_TARGET`, or the publish's generation shifted left once with
    /// the index of its copy in bit 0. The generation makes every publish a
    /// new value, so a compare-exchange cannot clear a newer edit.
    current: AtomicU64,
    generation: AtomicU64,
}

impl Default for PathTarget {
    fn default() -> Self {
        Self {
            copies: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            current: AtomicU64::new(NO_PATH_TARGET),
            generation: AtomicU64::new(0),
        }
    }
}

impl PathTarget {
    /// Editor thread only.
    fn publish(&self, order: &PathOrder) {
        let current = self.current.load(Ordering::Acquire);
        let index = if current == NO_PATH_TARGET {
            0
        } else {
            (current & 1) as usize ^ 1
        };
        // Release: a reader that sees one of these words also sees the
        // `current` change that took this copy out of use.
        for (cell, word) in self.copies[index].iter().zip(pack_path(order)) {
            cell.store(word, Ordering::Release);
        }
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.current
            .store(generation << 1 | index as u64, Ordering::Release);
    }

    /// The target and the `current` it was read under. `None` when there is
    /// none, or when the editor kept replacing it during a few tries, which
    /// the next block reads again. No allocation, no waiting.
    fn read(&self) -> Option<(u64, PathOrder)> {
        for _ in 0..4 {
            let current = self.current.load(Ordering::Acquire);
            if current == NO_PATH_TARGET {
                return None;
            }
            let copy = &self.copies[(current & 1) as usize];
            let words: PackedPath = std::array::from_fn(|i| copy[i].load(Ordering::Acquire));
            if self.current.load(Ordering::Acquire) == current {
                return Some((current, unpack_path(&words)));
            }
        }
        None
    }

    /// Clears the target if it is still `order`.
    fn finish(&self, order: &PathOrder) {
        if let Some((current, target)) = self.read() {
            if target == *order {
                let _ = self.current.compare_exchange(
                    current,
                    NO_PATH_TARGET,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                );
            }
        }
    }

    fn clear(&self) {
        self.current.store(NO_PATH_TARGET, Ordering::Release);
    }
}

pub struct Shared {
    in_peak: AtomicF32,
    in_rms: AtomicF32,
    out_peak: AtomicF32,
    out_rms: AtomicF32,
    in_clip: AtomicBool,
    out_clip: AtomicBool,

    telemetry: [AtomicF32; TELEMETRY_SLOTS],
    /// Bumped after each telemetry publish, so the editor redraws only on news.
    telemetry_seq: AtomicU32,

    sample_rate: AtomicU32,
    block_frames: AtomicU32,
    latency: AtomicU32,
    /// The host transport's tempo, 0 when it gives none.
    tempo: AtomicF32,

    /// Editor → audio: reset the sticky clip lights.
    pub clear_clip: AtomicBool,
    /// Editor → audio: the order a chain edit ends in, published before its
    /// first slot is written. See [`PathTarget`].
    path_target: PathTarget,

    /// The loaders of the running `Dsp`; `None` before `initialize`.
    pub control: Mutex<Option<ControlSide>>,
    /// Bumped each time `control` is replaced (a new sample rate, a new
    /// `Dsp`). A build started against the previous one must not submit.
    control_epoch: AtomicU64,
    /// Bumped when the DSP answered a capture or an IR load, so the editor
    /// re-reads that file's status.
    nam_answers: AtomicU32,
    ir_answers: AtomicU32,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            in_peak: AtomicF32::default(),
            in_rms: AtomicF32::default(),
            out_peak: AtomicF32::default(),
            out_rms: AtomicF32::default(),
            in_clip: AtomicBool::default(),
            out_clip: AtomicBool::default(),
            telemetry: std::array::from_fn(|_| AtomicF32::default()),
            telemetry_seq: AtomicU32::default(),
            sample_rate: AtomicU32::default(),
            block_frames: AtomicU32::default(),
            latency: AtomicU32::default(),
            tempo: AtomicF32::default(),
            clear_clip: AtomicBool::default(),
            path_target: PathTarget::default(),
            control: Mutex::new(None),
            control_epoch: AtomicU64::new(0),
            nam_answers: AtomicU32::default(),
            ir_answers: AtomicU32::default(),
        }
    }
}

impl Shared {
    /// Audio thread: this block's meter frame. Plain atomic stores.
    pub fn publish_meter(&self, m: MeterReading) {
        self.in_peak.store(m.in_peak, Ordering::Relaxed);
        self.in_rms.store(m.in_rms, Ordering::Relaxed);
        self.out_peak.store(m.out_peak, Ordering::Relaxed);
        self.out_rms.store(m.out_rms, Ordering::Relaxed);
        self.in_clip.store(m.in_clip, Ordering::Relaxed);
        self.out_clip.store(m.out_clip, Ordering::Relaxed);
    }

    pub fn meter(&self) -> MeterReading {
        MeterReading {
            in_peak: self.in_peak.load(Ordering::Relaxed),
            in_rms: self.in_rms.load(Ordering::Relaxed),
            out_peak: self.out_peak.load(Ordering::Relaxed),
            out_rms: self.out_rms.load(Ordering::Relaxed),
            in_clip: self.in_clip.load(Ordering::Relaxed),
            out_clip: self.out_clip.load(Ordering::Relaxed),
        }
    }

    /// Audio thread: the DSP's telemetry block (levels and the tuner).
    pub fn publish_telemetry(&self, slots: &[f32; TELEMETRY_SLOTS]) {
        for (cell, value) in self.telemetry.iter().zip(slots) {
            cell.store(*value, Ordering::Relaxed);
        }
        self.telemetry_seq.fetch_add(1, Ordering::Release);
    }

    /// `(sequence, slots)`, once anything was published.
    pub fn telemetry(&self) -> Option<(u32, [f32; TELEMETRY_SLOTS])> {
        let seq = self.telemetry_seq.load(Ordering::Acquire);
        (seq > 0).then(|| {
            (
                seq,
                std::array::from_fn(|i| self.telemetry[i].load(Ordering::Relaxed)),
            )
        })
    }

    pub fn set_stream(&self, sample_rate: f32, block_frames: u32) {
        self.sample_rate
            .store(sample_rate.round() as u32, Ordering::Relaxed);
        self.block_frames.store(block_frames, Ordering::Relaxed);
    }

    pub fn set_latency(&self, latency: u32) {
        self.latency.store(latency, Ordering::Relaxed);
    }

    pub fn set_tempo(&self, bpm: f32) {
        self.tempo.store(bpm, Ordering::Relaxed);
    }

    /// `(sample_rate, block_frames, latency, tempo)`.
    pub fn stream(&self) -> (u32, u32, u32, f32) {
        (
            self.sample_rate.load(Ordering::Relaxed),
            self.block_frames.load(Ordering::Relaxed),
            self.latency.load(Ordering::Relaxed),
            self.tempo.load(Ordering::Relaxed),
        )
    }

    /// Editor: the chain edit about to be written ends in `order`.
    /// Called from the editor thread only (the target's single writer).
    pub fn publish_path_target(&self, order: &PathOrder) {
        self.path_target.publish(order);
    }

    /// The order a chain edit in flight ends in. Audio thread: a few atomic
    /// loads, no allocation, no waiting.
    pub fn path_target(&self) -> Option<PathOrder> {
        self.path_target.read().map(|(_, order)| order)
    }

    /// Audio thread: the edit that ends in `order` is over (applied, or its
    /// slots went elsewhere). A newer edit's target stays.
    pub fn finish_path_target(&self, order: &PathOrder) {
        self.path_target.finish(order);
    }

    /// A state load or a new `Dsp` takes the chain as it stands.
    pub fn clear_path_target(&self) {
        self.path_target.clear();
    }

    /// Puts a new `Dsp`'s handles in place. Anything still building was
    /// aimed at the one replaced and drops itself; an editor pick it
    /// carried over is built again by the `restore` that follows.
    pub fn install_control(&self, nam: NamLoader, ir: IrLoader, sample_rate: f64) {
        let mut next = ControlSide::new(nam, ir, sample_rate);
        let mut guard = self.control.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = guard.as_mut() {
            next.nam_pick = previous.nam_pick.take();
            next.ir_pick = previous.ir_pick.take();
        }
        *guard = Some(next);
        self.bump_control_epoch();
    }

    pub fn bump_control_epoch(&self) {
        self.control_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub fn control_epoch(&self) -> u64 {
        self.control_epoch.load(Ordering::Acquire)
    }

    pub fn answered(&self, capture: bool) {
        let counter = if capture {
            &self.nam_answers
        } else {
            &self.ir_answers
        };
        counter.fetch_add(1, Ordering::Release);
    }

    /// `(capture answers, IR answers)` so far.
    pub fn answers(&self) -> (u32, u32) {
        (
            self.nam_answers.load(Ordering::Acquire),
            self.ir_answers.load(Ordering::Acquire),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every slot full, every stage used once (the last one in the last slot),
    /// and a chain with gaps.
    #[test]
    fn a_full_chain_of_every_stage_round_trips() {
        let full: PathOrder =
            std::array::from_fn(|slot| Some(StageKind::ALL[(slot + 1) % StageKind::ALL.len()]));
        assert_eq!(unpack_path(&pack_path(&full)), full);
        let mut gaps: PathOrder = [None; PATH_SLOTS];
        gaps[0] = Some(StageKind::Pitch);
        gaps[PATH_SLOTS - 1] = Some(StageKind::Gate);
        assert_eq!(unpack_path(&pack_path(&gaps)), gaps);

        let shared = Shared::default();
        assert_eq!(shared.path_target(), None);
        shared.publish_path_target(&full);
        assert_eq!(shared.path_target(), Some(full));
        // A newer edit replaces it in the other copy and keeps it.
        shared.publish_path_target(&gaps);
        shared.finish_path_target(&full);
        assert_eq!(shared.path_target(), Some(gaps));
        shared.finish_path_target(&gaps);
        assert_eq!(shared.path_target(), None);
        shared.publish_path_target(&full);
        shared.clear_path_target();
        assert_eq!(shared.path_target(), None);
    }
}

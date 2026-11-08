//! What the audio thread, the editor and the background task thread share.
//! The audio thread only ever touches the atomics here.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use nih_plug::prelude::AtomicF32;
use rodharerist::{IrLoader, NamLoader};

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

//! The realtime side: a session compiled into flat arrays the audio thread
//! walks once per block.
//!
//! The control thread builds a [`Graph`] whenever the *shape* of the mix
//! changes (a strip, an insert, a route, the patch) and hands it to the audio
//! thread whole; the old one comes back to be dropped off the audio thread.
//! Everything that moves while the mix plays — faders, pans, mutes, sends,
//! insert parameters, bypass — reaches the audio thread through the shared
//! cells below instead, so moving a fader never rebuilds anything.
//!
//! Effect instances live in [`InsertCell`]s that outlive any one graph: a new
//! graph reuses the cell of every insert that is still there, so rebuilding
//! never resets a reverb tail or a compressor's envelope. Each strip's own
//! processing section lives in a [`ProcessorCell`] the same way.
//!
//! Per strip, once per block:
//!
//! ```txt
//! talkback, oscillator: generated once, added into each destination's sum
//! playback: the loaded take's block, popped once (see `playback`)
//! channel: input (or, in virtual soundcheck, its playback file,
//!          crossfaded) → trim/polarity → [record "input"] → processing → inserts
//!          → [record "post_inserts"] → pre-fader sends (and sends with a
//!          pan of their own, or to a mono bus) → PFL cue
//!          → fader/pan/mute (DCAs, mute groups, solo in place)
//!          → meter → AFL cue → post-fader sends → bus or master
//! bus:     sum (→ (L+R)/2 when mono) → processing → inserts → PFL cue
//!          → fader → meter → AFL cue
//! master:  sum → processing → inserts → fader → meter
//! matrix:  Σ master and buses after their faders, each at its level and
//!          pan (→ (L+R)/2 when mono) → processing → inserts → PFL cue
//!          → fader → meter → AFL cue
//! monitor: Σ cues + the master (solo in place) or the monitor source
//!          (nothing soloed) → monitor level and dim → meter
//! ```

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use builtin_dsp_core::spectrum::SpectrumAnalyzer;

use crate::builtin_fx::BuiltinFx;
use crate::playback::PlayerCell;
use crate::processing::{Processing, ProcessingMeters, StripProcessor};
use crate::ring::SampleRing;
use crate::session::OscillatorKind;
use crate::telemetry::InsertTelemetry;

/// Largest block the graph processes at once. A device callback larger than
/// this is processed in pieces.
pub const MAX_BLOCK: usize = 1024;

/// How long switching an insert's bypass takes: the effect fades in or out
/// over this instead of cutting, which clicks.
pub const BYPASS_FADE_SECONDS: f32 = 0.010;

/// How long talkback and the oscillator take to come in or go out.
pub const INJECT_FADE_SECONDS: f32 = 0.050;

/// The talkback high-pass's corner.
pub const TALKBACK_HPF_HZ: f32 = 100.0;

/// An `f32` shared between threads.
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn store(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }
}

/// Peak level since the meter was last read, per side. The audio thread
/// raises it; the UI takes it (and resets it) at its own rate, so no peak
/// between two reads is ever missed.
#[derive(Debug, Default)]
pub struct Meter {
    peak_l: AtomicU32,
    peak_r: AtomicU32,
}

impl Meter {
    fn record(&self, left: &[f32], right: &[f32]) {
        let peak = |samples: &[f32]| samples.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        self.raise(peak(left), peak(right));
    }

    /// Raise the held peaks to at least `left` and `right` (≥ 0).
    fn raise(&self, left: f32, right: f32) {
        // Non-negative floats order the same as their bit patterns, so an
        // integer max is a float max.
        self.peak_l.fetch_max(left.to_bits(), Ordering::Relaxed);
        self.peak_r.fetch_max(right.to_bits(), Ordering::Relaxed);
    }

    /// `(left, right)` linear peaks since the last call.
    pub fn take(&self) -> (f32, f32) {
        (
            f32::from_bits(self.peak_l.swap(0, Ordering::Relaxed)),
            f32::from_bits(self.peak_r.swap(0, Ordering::Relaxed)),
        )
    }
}

/// What the control thread sets on a strip while it plays.
#[derive(Debug)]
pub struct StripShared {
    /// Final per-side gain: fader × DCAs × pan × (heard or not).
    pub gain_l: AtomicF32,
    pub gain_r: AtomicF32,
    /// Channel input gain (linear, sign carries the phase flip).
    pub trim: AtomicF32,
    /// How much of the strip goes to the monitor bus before its fader
    /// (`1` while soloed in PFL, else `0`).
    pub cue_pre: AtomicF32,
    /// The same, after its fader and pan (AFL).
    pub cue_post: AtomicF32,
    pub meter: Meter,
    /// Level entering the strip, after trim — what the input meter shows.
    pub input_meter: Meter,
    /// How much of a channel's input is its playback file rather than the
    /// interface (`0` live … `1` virtual soundcheck), as the audio thread
    /// last applied it: a new graph crossfades on from there.
    pub playback_mix: AtomicF32,
}

impl Default for StripShared {
    fn default() -> Self {
        Self {
            gain_l: AtomicF32::new(1.0),
            gain_r: AtomicF32::new(1.0),
            trim: AtomicF32::new(1.0),
            cue_pre: AtomicF32::new(0.0),
            cue_post: AtomicF32::new(0.0),
            meter: Meter::default(),
            input_meter: Meter::default(),
            playback_mix: AtomicF32::new(0.0),
        }
    }
}

/// What the control thread sets on the monitor bus.
#[derive(Debug)]
pub struct MonitorShared {
    /// Monitor level × dim (or talkback dim), linear.
    pub gain: AtomicF32,
    /// How much of the master the monitor bus carries: `1` in solo in place
    /// or (with the master as the monitor source) when nothing is soloed;
    /// `0` while PFL/AFL cues play.
    pub master_feed: AtomicF32,
    /// How much of the monitor source (a bus or matrix) it carries: `1`
    /// when nothing is soloed and the source is not the master.
    pub source_feed: AtomicF32,
    /// `(gain, master_feed, source_feed)` as the audio thread last applied
    /// them: a new graph's monitor ramps on from there.
    applied: [AtomicF32; 3],
    pub meter: Meter,
}

impl Default for MonitorShared {
    fn default() -> Self {
        Self {
            gain: AtomicF32::new(1.0),
            master_feed: AtomicF32::new(1.0),
            source_feed: AtomicF32::new(0.0),
            applied: [
                AtomicF32::new(1.0),
                AtomicF32::new(1.0),
                AtomicF32::new(0.0),
            ],
            meter: Meter::default(),
        }
    }
}

/// Per-side gains of a send or a matrix source, set by the control thread.
#[derive(Debug, Default)]
pub struct SendGains {
    pub left: AtomicF32,
    pub right: AtomicF32,
}

impl SendGains {
    pub fn new(left: f32, right: f32) -> Self {
        Self {
            left: AtomicF32::new(left),
            right: AtomicF32::new(right),
        }
    }

    pub fn load(&self) -> (f32, f32) {
        (self.left.load(), self.right.load())
    }

    pub fn store(&self, left: f32, right: f32) {
        self.left.store(left);
        self.right.store(right);
    }
}

/// How many settings snapshots may wait for the audio thread. Only the
/// newest matters; the sender makes room by dropping the oldest.
const PROCESSING_QUEUE: usize = 8;

/// One strip's processing section and its meters. Shared by successive
/// graphs, so a rebuild never resets a gate's envelope or a delay line.
pub struct ProcessorCell {
    processor: UnsafeCell<StripProcessor>,
    /// Settings waiting for the next block; the newest wins.
    settings: crossbeam_channel::Receiver<Processing>,
    gate_open: AtomicBool,
    /// Largest reductions since the control thread last took them, dB, as
    /// bits (non-negative floats order like their bits).
    gate_db: AtomicU32,
    comp_db: AtomicU32,
}

// SAFETY: `processor` is touched only by the audio thread (exactly one graph
// is live there at a time), or by the control thread before the cell is first
// published. Everything else is atomics and a channel.
unsafe impl Sync for ProcessorCell {}
unsafe impl Send for ProcessorCell {}

/// The control thread's end of a [`ProcessorCell`]'s settings queue.
pub struct ProcessorSender {
    tx: crossbeam_channel::Sender<Processing>,
    /// The same queue, to drop the oldest snapshot when it is full.
    rx: crossbeam_channel::Receiver<Processing>,
}

impl ProcessorSender {
    /// Queue `settings` for the audio thread. Never blocks; when the queue is
    /// full (the audio thread is not running), the oldest snapshot makes
    /// room, so the newest always arrives.
    pub fn send(&self, settings: Processing) {
        let mut settings = settings;
        loop {
            match self.tx.try_send(settings) {
                Ok(()) => return,
                Err(crossbeam_channel::TrySendError::Full(back)) => {
                    settings = back;
                    let _ = self.rx.try_recv();
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

impl ProcessorCell {
    /// A processor at `sample_rate`, already landed on `settings` (no glide
    /// from the defaults), and the sender its changes go through. Control
    /// thread; allocates.
    pub fn new(sample_rate: u32, settings: &Processing) -> (Arc<Self>, ProcessorSender) {
        let mut processor = StripProcessor::new(sample_rate);
        processor.set(settings);
        processor.settle();
        let (tx, rx) = crossbeam_channel::bounded(PROCESSING_QUEUE);
        (
            Arc::new(Self {
                processor: UnsafeCell::new(processor),
                settings: rx.clone(),
                gate_open: AtomicBool::new(true),
                gate_db: AtomicU32::new(0),
                comp_db: AtomicU32::new(0),
            }),
            ProcessorSender { tx, rx },
        )
    }

    /// The newest settings waiting, if any. Audio thread (or a test).
    pub(crate) fn take_latest(&self) -> Option<Processing> {
        let mut latest = None;
        while let Ok(settings) = self.settings.try_recv() {
            latest = Some(settings);
        }
        latest
    }

    /// One block in place. Audio thread only.
    fn process(&self, left: &mut [f32], right: &mut [f32], stereo: bool) {
        // SAFETY: see the type's contract.
        let processor = unsafe { &mut *self.processor.get() };
        if let Some(settings) = self.take_latest() {
            processor.set(&settings);
        }
        processor.process(left, right, stereo);
        let meters = processor.meters();
        self.gate_open.store(meters.gate_open, Ordering::Relaxed);
        // `max` turns a NaN into 0.
        self.gate_db
            .fetch_max(meters.gate_db.max(0.0).to_bits(), Ordering::Relaxed);
        self.comp_db
            .fetch_max(meters.comp_db.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// The gate's state and the largest reductions since the last call.
    /// Control thread.
    pub fn take_meters(&self) -> ProcessingMeters {
        ProcessingMeters {
            gate_open: self.gate_open.load(Ordering::Relaxed),
            gate_db: f32::from_bits(self.gate_db.swap(0, Ordering::Relaxed)),
            comp_db: f32::from_bits(self.comp_db.swap(0, Ordering::Relaxed)),
        }
    }
}

/// The DSP of one insert.
pub enum InsertDsp {
    Builtin(BuiltinFx),
    #[cfg(feature = "external-plugins")]
    External(crate::external::ExternalInsert),
}

/// The crossfade between an insert's input and its effect when the bypass
/// changes. Audio thread only.
struct BypassFade {
    /// How much of the effect is heard: `0` bypassed … `1` in.
    wet: f32,
    /// `wet`'s change per sample while fading.
    step: f32,
    /// The block's input, kept for the fade.
    dry_l: Box<[f32]>,
    dry_r: Box<[f32]>,
}

/// One insert's effect and its live controls. Shared by successive graphs.
pub struct InsertCell {
    dsp: UnsafeCell<InsertDsp>,
    fade: UnsafeCell<BypassFade>,
    /// What arrives at a built-in, for its editor's analyser. Fed only while
    /// [`InsertTelemetry::watched`].
    analyzer: UnsafeCell<Option<SpectrumAnalyzer>>,
    pub bypass: AtomicBool,
    /// Wire parameter changes waiting for the next block.
    params: crossbeam_channel::Receiver<(u32, f32)>,
    pub telemetry: InsertTelemetry,
}

// SAFETY: `dsp`, `fade` and `analyzer` are touched only by the audio thread (exactly
// one graph is live there at a time), or by the control thread before the
// cell is first published. Everything else is atomics and a channel.
unsafe impl Sync for InsertCell {}
unsafe impl Send for InsertCell {}

impl InsertCell {
    /// The cell, and the sender its parameter changes go through.
    pub fn new(
        dsp: InsertDsp,
        bypass: bool,
        sample_rate: u32,
    ) -> (Arc<Self>, crossbeam_channel::Sender<(u32, f32)>) {
        let (tx, rx) = crossbeam_channel::bounded(1024);
        // Allocated here, on the control thread, never on the audio one.
        let analyzer = matches!(dsp, InsertDsp::Builtin(_))
            .then(|| SpectrumAnalyzer::new(sample_rate.max(1) as f32));
        (
            Arc::new(Self {
                dsp: UnsafeCell::new(dsp),
                fade: UnsafeCell::new(BypassFade {
                    // Starts where the bypass is: no fade on load.
                    wet: if bypass { 0.0 } else { 1.0 },
                    step: 1.0 / (BYPASS_FADE_SECONDS * sample_rate.max(1) as f32).max(1.0),
                    dry_l: vec![0.0; MAX_BLOCK].into_boxed_slice(),
                    dry_r: vec![0.0; MAX_BLOCK].into_boxed_slice(),
                }),
                analyzer: UnsafeCell::new(analyzer),
                bypass: AtomicBool::new(bypass),
                params: rx,
                telemetry: InsertTelemetry::default(),
            }),
            tx,
        )
    }

    /// Audio thread only.
    fn process(&self, left: &mut [f32], right: &mut [f32]) {
        // SAFETY: see the type's contract.
        let dsp = unsafe { &mut *self.dsp.get() };
        while let Ok((index, value)) = self.params.try_recv() {
            // Irrefutable in a build without third-party plug-ins.
            #[allow(irrefutable_let_patterns)]
            if let InsertDsp::Builtin(fx) = dsp {
                fx.apply_wire_param(index, value);
            }
        }
        // SAFETY: see the type's contract.
        let fade = unsafe { &mut *self.fade.get() };
        let target = if self.bypass.load(Ordering::Relaxed) {
            0.0
        } else {
            1.0
        };
        if fade.wet == target {
            if target == 0.0 {
                return;
            }
            self.run(dsp, left, right);
            return;
        }
        // Switching: run the effect and move from what was heard to what
        // will be, sample by sample.
        let n = left.len().min(MAX_BLOCK);
        fade.dry_l[..n].copy_from_slice(&left[..n]);
        fade.dry_r[..n].copy_from_slice(&right[..n]);
        self.run(dsp, left, right);
        for i in 0..n {
            fade.wet = if target > fade.wet {
                (fade.wet + fade.step).min(1.0)
            } else {
                (fade.wet - fade.step).max(0.0)
            };
            left[i] = fade.dry_l[i] + (left[i] - fade.dry_l[i]) * fade.wet;
            right[i] = fade.dry_r[i] + (right[i] - fade.dry_r[i]) * fade.wet;
        }
    }

    /// The effect itself, in place. Audio thread only.
    fn run(&self, dsp: &mut InsertDsp, left: &mut [f32], right: &mut [f32]) {
        match dsp {
            InsertDsp::Builtin(fx) => {
                if self.telemetry.is_watched() {
                    // SAFETY: see the type's contract.
                    let analyzer = unsafe { &mut *self.analyzer.get() }.as_mut();
                    let analyzer = analyzer.map(|a| {
                        a.push_block(left, right);
                        a
                    });
                    fx.process(left, right);
                    self.telemetry.publish(fx, analyzer);
                } else {
                    fx.process(left, right);
                }
            }
            #[cfg(feature = "external-plugins")]
            InsertDsp::External(external) => external.process(left, right),
        }
    }
}

/// Where a strip's post-fader signal goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Master,
    Bus(usize),
    None,
}

/// A mix after its fader: what a matrix sums and the monitor bus plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixFrom {
    Master,
    Bus(usize),
    Matrix(usize),
}

/// Where talkback or the oscillator is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectTo {
    Master,
    Bus(usize),
    Matrix(usize),
    Monitor,
}

/// A second-order Butterworth high-pass (RBJ), transposed direct form II.
struct HighPass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl HighPass {
    fn new(hz: f32, sample_rate: f32) -> Self {
        let w0 = std::f32::consts::TAU * (hz / sample_rate.max(1.0)).min(0.49);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 + cos) / 2.0 / a0,
            b1: -(1.0 + cos) / a0,
            b2: (1.0 + cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        // Never let a denormal or a NaN settle in the state (`!(x > ε)` is
        // true for a NaN too).
        if !(self.z1.abs() > 1e-25) {
            self.z1 = 0.0;
        }
        if !(self.z2.abs() > 1e-25) {
            self.z2 = 0.0;
        }
        y
    }
}

/// Moves `value` one `step` towards `target`, landing on it exactly.
fn approach(value: f32, target: f32, step: f32) -> f32 {
    if value < target {
        (value + step).min(target)
    } else {
        (value - step).max(target)
    }
}

/// Talkback's live state on the audio thread, kept across graph rebuilds.
struct TalkbackState {
    /// `0` silent … `1` talking: the switch, ramped.
    env: f32,
    env_step: f32,
    /// The level applied at the end of the last block.
    level: f32,
    /// `0` the HPF out … `1` in, ramped.
    hpf_mix: f32,
    hpf_step: f32,
    hpf: HighPass,
}

/// The talkback mic: its switch, level and HPF, and its meter. Shared by
/// successive graphs, so a rebuild never cuts a word or restarts a ramp.
pub struct TalkbackCell {
    /// Linear.
    pub level: AtomicF32,
    pub active: AtomicBool,
    pub hpf: AtomicBool,
    /// Talkback after its HPF and level, whether talking or not: the mic can
    /// be checked before talk is pressed.
    pub meter: Meter,
    state: UnsafeCell<TalkbackState>,
}

// SAFETY: `state` is touched only by the audio thread (exactly one graph is
// live there at a time). Everything else is atomics.
unsafe impl Sync for TalkbackCell {}
unsafe impl Send for TalkbackCell {}

impl TalkbackCell {
    /// Off, at unity, HPF in. Control thread; allocates.
    pub fn new(sample_rate: u32) -> Arc<Self> {
        let sr = sample_rate.max(1) as f32;
        Arc::new(Self {
            level: AtomicF32::new(1.0),
            active: AtomicBool::new(false),
            hpf: AtomicBool::new(true),
            meter: Meter::default(),
            state: UnsafeCell::new(TalkbackState {
                env: 0.0,
                env_step: 1.0 / (INJECT_FADE_SECONDS * sr).max(1.0),
                level: 1.0,
                hpf_mix: 1.0,
                hpf_step: 1.0 / (BYPASS_FADE_SECONDS * sr).max(1.0),
                hpf: HighPass::new(TALKBACK_HPF_HZ, sr),
            }),
        })
    }

    /// One block of talkback from `input` into `out`. `false` when it is
    /// silent (nothing to add). Audio thread only.
    fn render(&self, input: Option<&[f32]>, out: &mut [f32]) -> bool {
        // SAFETY: see the type's contract.
        let state = unsafe { &mut *self.state.get() };
        let n = out.len();
        let level = self.level.load();
        let start = state.level;
        state.level = level;
        let env_target = if self.active.load(Ordering::Relaxed) {
            1.0
        } else {
            0.0
        };
        let hpf_target = if self.hpf.load(Ordering::Relaxed) {
            1.0
        } else {
            0.0
        };
        let Some(input) = input else {
            state.env = env_target;
            state.hpf_mix = hpf_target;
            out.fill(0.0);
            return false;
        };
        let level_step = (level - start) / n.max(1) as f32;
        let mut peak = 0.0f32;
        let mut heard = env_target > 0.0 || state.env > 0.0;
        for (i, sample) in out.iter_mut().enumerate() {
            let x = input.get(i).copied().unwrap_or(0.0);
            // Always run, so the filter is settled whenever it is switched in.
            let filtered = state.hpf.process(x);
            state.hpf_mix = approach(state.hpf_mix, hpf_target, state.hpf_step);
            let s = (x + (filtered - x) * state.hpf_mix) * (start + level_step * (i + 1) as f32);
            peak = peak.max(s.abs());
            state.env = approach(state.env, env_target, state.env_step);
            *sample = s * state.env;
        }
        self.meter.raise(peak, peak);
        heard &= peak > 0.0;
        heard
    }
}

/// The oscillator's generator state on the audio thread, kept across graph
/// rebuilds.
struct OscillatorState {
    sample_rate: f32,
    env: f32,
    env_step: f32,
    level: f32,
    /// What it plays now; a change of kind fades out, switches, fades in.
    kind: u32,
    /// Sine phase, in cycles.
    phase: f64,
    /// xorshift32.
    rng: u32,
    /// Paul Kellet's pink filter.
    pink: [f32; 7],
}

impl OscillatorState {
    fn white(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x as i32) as f32 / 2_147_483_648.0
    }

    fn next(&mut self, hz: f32) -> f32 {
        match self.kind {
            OSC_PINK => {
                let white = self.white();
                let b = &mut self.pink;
                b[0] = 0.99886 * b[0] + white * 0.055_517_9;
                b[1] = 0.99332 * b[1] + white * 0.075_075_9;
                b[2] = 0.96900 * b[2] + white * 0.153_852;
                b[3] = 0.86650 * b[3] + white * 0.310_485_6;
                b[4] = 0.55000 * b[4] + white * 0.532_952_2;
                b[5] = -0.7616 * b[5] - white * 0.016_898;
                let pink = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + white * 0.5362;
                b[6] = white * 0.115_926;
                (pink * 0.11).clamp(-1.0, 1.0)
            }
            OSC_WHITE => self.white(),
            _ => {
                let s = (self.phase * std::f64::consts::TAU).sin() as f32;
                self.phase += f64::from(hz) / f64::from(self.sample_rate);
                self.phase -= self.phase.floor();
                s
            }
        }
    }
}

const OSC_SINE: u32 = 0;
const OSC_PINK: u32 = 1;
const OSC_WHITE: u32 = 2;

/// The test oscillator: kind, frequency, level and on/off, set by the
/// control thread. Shared by successive graphs.
pub struct OscillatorCell {
    /// Peak level, linear.
    pub level: AtomicF32,
    pub on: AtomicBool,
    kind: AtomicU32,
    pub hz: AtomicF32,
    state: UnsafeCell<OscillatorState>,
}

// SAFETY: `state` is touched only by the audio thread (exactly one graph is
// live there at a time). Everything else is atomics.
unsafe impl Sync for OscillatorCell {}
unsafe impl Send for OscillatorCell {}

impl OscillatorCell {
    /// Off, a 1 kHz sine. Control thread; allocates.
    pub fn new(sample_rate: u32) -> Arc<Self> {
        let sr = sample_rate.max(1) as f32;
        Arc::new(Self {
            level: AtomicF32::new(0.1),
            on: AtomicBool::new(false),
            kind: AtomicU32::new(OSC_SINE),
            hz: AtomicF32::new(1000.0),
            state: UnsafeCell::new(OscillatorState {
                sample_rate: sr,
                env: 0.0,
                env_step: 1.0 / (INJECT_FADE_SECONDS * sr).max(1.0),
                level: 0.1,
                kind: OSC_SINE,
                phase: 0.0,
                rng: 0x9E37_79B9,
                pink: [0.0; 7],
            }),
        })
    }

    pub fn set_kind(&self, kind: OscillatorKind) {
        let code = match kind {
            OscillatorKind::Sine => OSC_SINE,
            OscillatorKind::Pink => OSC_PINK,
            OscillatorKind::White => OSC_WHITE,
        };
        self.kind.store(code, Ordering::Relaxed);
    }

    /// One block into `out`. `false` when it is silent (off and faded out).
    /// Audio thread only.
    fn render(&self, out: &mut [f32]) -> bool {
        // SAFETY: see the type's contract.
        let state = unsafe { &mut *self.state.get() };
        let n = out.len();
        let level = self.level.load();
        let start = state.level;
        state.level = level;
        let on = self.on.load(Ordering::Relaxed);
        let wanted = self.kind.load(Ordering::Relaxed);
        if !on && state.env == 0.0 {
            state.kind = wanted;
            state.phase = 0.0;
            return false;
        }
        let hz = self.hz.load();
        let hz = if hz.is_finite() {
            hz.min(state.sample_rate * 0.49).max(0.0)
        } else {
            1000.0
        };
        let level_step = (level - start) / n.max(1) as f32;
        for (i, sample) in out.iter_mut().enumerate() {
            let env_target = if on && state.kind == wanted { 1.0 } else { 0.0 };
            state.env = approach(state.env, env_target, state.env_step);
            if state.env == 0.0 && state.kind != wanted {
                state.kind = wanted;
                state.phase = 0.0;
            }
            let s = state.next(hz);
            *sample = s * state.env * (start + level_step * (i + 1) as f32);
        }
        true
    }
}

/// Talkback in a graph: its cell, its input and where it goes.
pub struct RtTalkback {
    pub cell: Arc<TalkbackCell>,
    /// The interface input (in range), if any.
    pub input: Option<usize>,
    pub to: Vec<InjectTo>,
}

/// The oscillator in a graph: its cell and where it goes.
pub struct RtOscillator {
    pub cell: Arc<OscillatorCell>,
    pub to: Vec<InjectTo>,
}

/// The playback file a channel can take its input from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackTap {
    /// The file's index in the take.
    pub track: usize,
    /// The channel is stereo: a stereo file plays L/R (a mono one on both
    /// sides). A mono channel takes (L+R)/2.
    pub stereo: bool,
}

/// A loaded take in a graph: its cell and, per channel (in graph order),
/// the file it can take its input from.
pub struct RtPlayback {
    pub cell: Arc<PlayerCell>,
    pub taps: Vec<Option<PlaybackTap>>,
}

/// A strip's recorded signal, on its way to the disk writer.
pub struct RecordStream {
    pub ring: SampleRing,
    /// 1 for a mono channel, 2 otherwise.
    pub channels: usize,
    /// Samples the writer could not keep up with.
    pub dropped: AtomicU64,
}

impl RecordStream {
    pub fn new(channels: usize, seconds: usize, sample_rate: u32) -> Self {
        Self {
            ring: SampleRing::new(channels * seconds * sample_rate.max(1) as usize),
            channels,
            dropped: AtomicU64::new(0),
        }
    }

    fn write(&self, left: &[f32], right: &[f32], scratch: &mut [f32]) {
        let frames = left.len();
        let samples = if self.channels == 1 {
            scratch[..frames].copy_from_slice(left);
            &scratch[..frames]
        } else {
            for i in 0..frames {
                scratch[i * 2] = left[i];
                scratch[i * 2 + 1] = right[i];
            }
            &scratch[..frames * 2]
        };
        let pushed = self.ring.push(samples);
        if pushed < samples.len() {
            self.dropped
                .fetch_add((samples.len() - pushed) as u64, Ordering::Relaxed);
        }
    }
}

pub struct RtStrip {
    pub shared: Arc<StripShared>,
    pub processor: Arc<ProcessorCell>,
    pub inserts: Vec<Arc<InsertCell>>,
    pub record: Option<Arc<RecordStream>>,
    /// Gains applied at the end of the last block; ramped to the targets.
    current: (f32, f32),
    /// Cue gains (PFL, AFL) at the end of the last block.
    cue_current: (f32, f32),
    left: Vec<f32>,
    right: Vec<f32>,
}

impl RtStrip {
    pub fn new(
        shared: Arc<StripShared>,
        processor: Arc<ProcessorCell>,
        inserts: Vec<Arc<InsertCell>>,
        record: Option<Arc<RecordStream>>,
    ) -> Self {
        let current = (shared.gain_l.load(), shared.gain_r.load());
        let cue_current = (shared.cue_pre.load(), shared.cue_post.load());
        Self {
            shared,
            processor,
            inserts,
            record,
            current,
            cue_current,
            left: vec![0.0; MAX_BLOCK],
            right: vec![0.0; MAX_BLOCK],
        }
    }

    /// The strip's processing section, then its inserts. A mono strip
    /// (`stereo == false`) carries its signal in `left`; the processed
    /// signal is copied to `right` before the inserts, as the graph keeps a
    /// mono strip's two sides equal.
    fn run_processing_and_inserts(&mut self, n: usize, stereo: bool) {
        self.processor
            .process(&mut self.left[..n], &mut self.right[..n], stereo);
        if !stereo {
            self.right[..n].copy_from_slice(&self.left[..n]);
        }
        for insert in &self.inserts {
            insert.process(&mut self.left[..n], &mut self.right[..n]);
        }
    }

    /// Add the strip's current signal to the monitor bus at its PFL
    /// (`pre == true`) or AFL cue gain, ramped across the block.
    fn cue_into(&mut self, monitor: &mut RtMonitor, pre: bool, n: usize) {
        let (target, start) = if pre {
            (self.shared.cue_pre.load(), self.cue_current.0)
        } else {
            (self.shared.cue_post.load(), self.cue_current.1)
        };
        if pre {
            self.cue_current.0 = target;
        } else {
            self.cue_current.1 = target;
        }
        if start == 0.0 && target == 0.0 {
            return;
        }
        let step = (target - start) / n.max(1) as f32;
        for i in 0..n {
            let gain = start + step * (i + 1) as f32;
            monitor.left[i] += self.left[i] * gain;
            monitor.right[i] += self.right[i] * gain;
        }
    }

    /// Fader, pan and mute, ramped across the block so a move never clicks.
    fn apply_gain(&mut self, n: usize) {
        let target = (self.shared.gain_l.load(), self.shared.gain_r.load());
        let (start_l, start_r) = self.current;
        let step_l = (target.0 - start_l) / n.max(1) as f32;
        let step_r = (target.1 - start_r) / n.max(1) as f32;
        for i in 0..n {
            self.left[i] *= start_l + step_l * (i + 1) as f32;
            self.right[i] *= start_r + step_r * (i + 1) as f32;
        }
        self.current = target;
    }

    /// A mono bus or matrix: both sides become (L+R)/2.
    fn fold_to_mono(&mut self, n: usize) {
        for i in 0..n {
            let mono = (self.left[i] + self.right[i]) * 0.5;
            self.left[i] = mono;
            self.right[i] = mono;
        }
    }

    /// Everything after the sum, for a bus or a matrix: processing,
    /// inserts, PFL cue, fader, meter, AFL cue, recording.
    fn finish_mix(&mut self, stereo: bool, monitor: &mut RtMonitor, scratch: &mut [f32], n: usize) {
        if !stereo {
            self.fold_to_mono(n);
        }
        self.run_processing_and_inserts(n, stereo);
        self.cue_into(monitor, true, n);
        self.apply_gain(n);
        self.shared.meter.record(&self.left[..n], &self.right[..n]);
        self.cue_into(monitor, false, n);
        if let Some(record) = &self.record {
            record.write(&self.left[..n], &self.right[..n], scratch);
        }
    }
}

/// Where a send takes the channel's signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendTap {
    /// After the inserts, before the fader. Post-fader sends with a pan of
    /// their own (or to a mono bus) are taken here too, with the channel's
    /// level in their gains, as they must not take the channel's pan.
    BeforeFader,
    /// After the fader and the channel's pan.
    AfterFader,
}

pub struct RtSend {
    pub bus: usize,
    pub gains: Arc<SendGains>,
    pub tap: SendTap,
    current: (f32, f32),
}

impl RtSend {
    pub fn new(bus: usize, gains: Arc<SendGains>, tap: SendTap) -> Self {
        let current = gains.load();
        Self {
            bus,
            gains,
            tap,
            current,
        }
    }
}

/// One contribution to a matrix on the audio thread.
pub struct RtMatrixSource {
    pub from: MixFrom,
    pub gains: Arc<SendGains>,
    current: (f32, f32),
}

impl RtMatrixSource {
    pub fn new(from: MixFrom, gains: Arc<SendGains>) -> Self {
        let current = gains.load();
        Self {
            from,
            gains,
            current,
        }
    }
}

pub struct RtMatrix {
    pub strip: RtStrip,
    pub stereo: bool,
    pub sources: Vec<RtMatrixSource>,
}

pub struct RtChannel {
    pub strip: RtStrip,
    pub input_left: Option<usize>,
    pub input_right: Option<usize>,
    pub sends: Vec<RtSend>,
    pub dest: Dest,
    /// Record the input (after trim) rather than after the inserts.
    pub record_input: bool,
}

pub struct RtBus {
    pub strip: RtStrip,
    pub dest: Dest,
    /// `false`: a mono bus, its sum folded to (L+R)/2.
    pub stereo: bool,
}

/// The monitor bus on the audio thread.
pub struct RtMonitor {
    pub shared: Arc<MonitorShared>,
    /// What it plays at [`MonitorShared::source_feed`].
    source: MixFrom,
    /// The source the last graph played: crossfaded from in this graph's
    /// first block.
    fade_from: Option<MixFrom>,
    /// `(gain, master_feed, source_feed)` at the end of the last block.
    current: (f32, f32, f32),
    left: Vec<f32>,
    right: Vec<f32>,
}

impl RtMonitor {
    pub fn new(shared: Arc<MonitorShared>) -> Self {
        let current = (
            shared.applied[0].load(),
            shared.applied[1].load(),
            shared.applied[2].load(),
        );
        Self {
            shared,
            source: MixFrom::Master,
            fade_from: None,
            current,
            left: vec![0.0; MAX_BLOCK],
            right: vec![0.0; MAX_BLOCK],
        }
    }

    /// Play `source` at the source feed; `previous` (another source the last
    /// graph played) is crossfaded out over the first block.
    pub fn with_source(mut self, source: MixFrom, previous: Option<MixFrom>) -> Self {
        self.source = source;
        self.fade_from = previous.filter(|p| *p != source);
        self
    }

    /// The cues (and injections) are in; add the master and the source as
    /// far as each is fed, then the monitor level, each ramped across the
    /// block. A source that is missing is silence.
    fn finish(
        &mut self,
        master: (&[f32], &[f32]),
        source: Option<(&[f32], &[f32])>,
        previous: Option<(&[f32], &[f32])>,
        n: usize,
    ) {
        let gain = self.shared.gain.load();
        let feed = self.shared.master_feed.load();
        let source_feed = self.shared.source_feed.load();
        let (start_gain, start_feed, start_source) = self.current;
        self.current = (gain, feed, source_feed);
        let steps = n.max(1) as f32;
        let gain_step = (gain - start_gain) / steps;
        let feed_step = (feed - start_feed) / steps;
        let source_step = (source_feed - start_source) / steps;
        let fading = self.fade_from.take().is_some();
        for i in 0..n {
            let at = (i + 1) as f32;
            let g = start_gain + gain_step * at;
            let f = start_feed + feed_step * at;
            let s = start_source + source_step * at;
            let (mut l, mut r) = (
                self.left[i] + master.0[i] * f,
                self.right[i] + master.1[i] * f,
            );
            // The new source in, the old one out, across the first block.
            let fade_in = if fading { at / steps } else { 1.0 };
            if let Some((sl, sr)) = source {
                l += sl[i] * s * fade_in;
                r += sr[i] * s * fade_in;
            }
            if let (true, Some((pl, pr))) = (fading, previous) {
                l += pl[i] * s * (1.0 - fade_in);
                r += pr[i] * s * (1.0 - fade_in);
            }
            self.left[i] = l * g;
            self.right[i] = r * g;
        }
        for (applied, value) in self.shared.applied.iter().zip([gain, feed, source_feed]) {
            applied.store(value);
        }
        self.shared.meter.record(&self.left[..n], &self.right[..n]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchFrom {
    Master,
    Bus(usize),
    Channel(usize),
    Monitor,
    Matrix(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct RtPatch {
    pub from: PatchFrom,
    pub left: usize,
    pub right: Option<usize>,
}

pub struct Graph {
    /// Which build this is; the audio thread reports the one it runs.
    pub generation: u64,
    pub in_channels: usize,
    pub out_channels: usize,
    pub channels: Vec<RtChannel>,
    pub buses: Vec<RtBus>,
    pub master: RtStrip,
    pub matrices: Vec<RtMatrix>,
    pub monitor: RtMonitor,
    pub patches: Vec<RtPatch>,
    pub talkback: Option<RtTalkback>,
    pub oscillator: Option<RtOscillator>,
    pub playback: Option<RtPlayback>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    record_scratch: Vec<f32>,
    /// One block of talkback or oscillator, on its way to its destinations.
    inject: Vec<f32>,
}

impl Graph {
    pub fn new(
        in_channels: usize,
        out_channels: usize,
        channels: Vec<RtChannel>,
        buses: Vec<RtBus>,
        master: RtStrip,
        monitor: RtMonitor,
        patches: Vec<RtPatch>,
    ) -> Self {
        Self {
            generation: 0,
            in_channels,
            out_channels,
            channels,
            buses,
            master,
            matrices: Vec::new(),
            monitor,
            patches,
            talkback: None,
            oscillator: None,
            playback: None,
            inputs: (0..in_channels).map(|_| vec![0.0; MAX_BLOCK]).collect(),
            outputs: (0..out_channels).map(|_| vec![0.0; MAX_BLOCK]).collect(),
            record_scratch: vec![0.0; MAX_BLOCK * 2],
            inject: vec![0.0; MAX_BLOCK],
        }
    }

    /// The graph with `matrices`, after the master.
    pub fn with_matrices(mut self, matrices: Vec<RtMatrix>) -> Self {
        self.matrices = matrices;
        self
    }

    pub fn with_talkback(mut self, talkback: RtTalkback) -> Self {
        self.talkback = Some(talkback);
        self
    }

    pub fn with_oscillator(mut self, oscillator: RtOscillator) -> Self {
        self.oscillator = Some(oscillator);
        self
    }

    pub fn with_playback(mut self, playback: RtPlayback) -> Self {
        self.playback = Some(playback);
        self
    }

    /// Render one device callback: `input` is interleaved at
    /// [`Self::in_channels`] (shorter than the block is silence), `output`
    /// interleaved at [`Self::out_channels`]. Audio thread; allocation-free.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let out_ch = self.out_channels.max(1);
        let frames = output.len() / out_ch;
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            let in_ch = self.in_channels;
            let in_start = done * in_ch;
            let in_end = ((done + n) * in_ch).min(input.len());
            let input_block = if in_start < in_end {
                &input[in_start..in_end]
            } else {
                &[][..]
            };
            self.process_block(input_block, n);
            let out_block = &mut output[done * out_ch..(done + n) * out_ch];
            for (ch, plane) in self.outputs.iter().enumerate() {
                for i in 0..n {
                    out_block[i * out_ch + ch] = plane[i];
                }
            }
            done += n;
        }
    }

    fn process_block(&mut self, input: &[f32], n: usize) {
        let in_ch = self.in_channels;
        let available = if in_ch == 0 { 0 } else { input.len() / in_ch };
        for (ch, plane) in self.inputs.iter_mut().enumerate() {
            for (i, sample) in plane[..n].iter_mut().enumerate() {
                *sample = if i < available {
                    input[i * in_ch + ch]
                } else {
                    0.0
                };
            }
        }

        for bus in &mut self.buses {
            bus.strip.left[..n].fill(0.0);
            bus.strip.right[..n].fill(0.0);
        }
        for matrix in &mut self.matrices {
            matrix.strip.left[..n].fill(0.0);
            matrix.strip.right[..n].fill(0.0);
        }
        self.master.left[..n].fill(0.0);
        self.master.right[..n].fill(0.0);
        self.monitor.left[..n].fill(0.0);
        self.monitor.right[..n].fill(0.0);

        // Talkback and the oscillator, into their destinations' sums.
        if let Some(talkback) = &self.talkback {
            let input = talkback.input.and_then(|i| self.inputs.get(i));
            let block = &mut self.inject[..n];
            if talkback.cell.render(input.map(|plane| &plane[..n]), block) {
                inject(
                    &talkback.to,
                    block,
                    &mut self.master,
                    &mut self.buses,
                    &mut self.matrices,
                    &mut self.monitor,
                );
            }
        }
        if let Some(oscillator) = &self.oscillator {
            let block = &mut self.inject[..n];
            if oscillator.cell.render(block) {
                inject(
                    &oscillator.to,
                    block,
                    &mut self.master,
                    &mut self.buses,
                    &mut self.matrices,
                    &mut self.monitor,
                );
            }
        }

        // The loaded take's block, popped once for every channel it feeds.
        let playback = self.playback.as_ref();
        let virtual_soundcheck = playback.is_some_and(|p| {
            p.cell.render(n);
            p.cell.virtual_soundcheck()
        });

        for (index, channel) in self.channels.iter_mut().enumerate() {
            let strip = &mut channel.strip;
            let trim = strip.shared.trim.load();
            let mut stereo = false;
            match (channel.input_left, channel.input_right) {
                (Some(l), right) if l < self.inputs.len() => {
                    let right = right.filter(|r| *r < self.inputs.len());
                    stereo = right.is_some();
                    let r = right.unwrap_or(l);
                    for i in 0..n {
                        strip.left[i] = self.inputs[l][i] * trim;
                        strip.right[i] = self.inputs[r][i] * trim;
                    }
                }
                _ => {
                    strip.left[..n].fill(0.0);
                    strip.right[..n].fill(0.0);
                }
            }
            // Virtual soundcheck: the playback file in place of the
            // interface input (before trim, like it), crossfaded.
            let tap = playback.and_then(|p| p.taps.get(index).copied().flatten());
            let start = strip.shared.playback_mix.load();
            let target = if virtual_soundcheck && tap.is_some() {
                1.0
            } else {
                0.0
            };
            if start != 0.0 || target != 0.0 {
                match (playback, tap) {
                    (Some(p), Some(tap)) => {
                        let (from_l, from_r) = p.cell.planes(tap.track, n);
                        stereo |= tap.stereo;
                        let mut mix = start;
                        for i in 0..n {
                            mix = approach(mix, target, p.cell.fade_step);
                            let (l, r) = if tap.stereo {
                                (from_l[i], from_r[i])
                            } else {
                                let mono = (from_l[i] + from_r[i]) * 0.5;
                                (mono, mono)
                            };
                            strip.left[i] += (l * trim - strip.left[i]) * mix;
                            strip.right[i] += (r * trim - strip.right[i]) * mix;
                        }
                        strip.shared.playback_mix.store(mix);
                    }
                    // The file is gone (unassigned or unloaded): back to
                    // the live input, faded in from silence when it can be.
                    _ => {
                        let mut mix = start;
                        match playback {
                            Some(p) => {
                                for i in 0..n {
                                    mix = approach(mix, 0.0, p.cell.fade_step);
                                    strip.left[i] *= 1.0 - mix;
                                    strip.right[i] *= 1.0 - mix;
                                }
                            }
                            None => mix = 0.0,
                        }
                        strip.shared.playback_mix.store(mix);
                    }
                }
            }
            strip
                .shared
                .input_meter
                .record(&strip.left[..n], &strip.right[..n]);
            if channel.record_input {
                if let Some(record) = &strip.record {
                    record.write(
                        &strip.left[..n],
                        &strip.right[..n],
                        &mut self.record_scratch,
                    );
                }
            }
            strip.run_processing_and_inserts(n, stereo);
            if !channel.record_input {
                if let Some(record) = &strip.record {
                    record.write(
                        &strip.left[..n],
                        &strip.right[..n],
                        &mut self.record_scratch,
                    );
                }
            }
            send_into(
                &mut channel.sends,
                SendTap::BeforeFader,
                strip,
                &mut self.buses,
                n,
            );
            strip.cue_into(&mut self.monitor, true, n);
            strip.apply_gain(n);
            strip
                .shared
                .meter
                .record(&strip.left[..n], &strip.right[..n]);
            strip.cue_into(&mut self.monitor, false, n);
            send_into(
                &mut channel.sends,
                SendTap::AfterFader,
                strip,
                &mut self.buses,
                n,
            );
            match channel.dest {
                Dest::Master => mix_into(&mut self.master, strip, n),
                Dest::Bus(index) => {
                    if let Some(bus) = self.buses.get_mut(index) {
                        mix_into(&mut bus.strip, strip, n);
                    }
                }
                Dest::None => {}
            }
        }

        for bus in &mut self.buses {
            let strip = &mut bus.strip;
            strip.finish_mix(bus.stereo, &mut self.monitor, &mut self.record_scratch, n);
            if bus.dest == Dest::Master {
                mix_into(&mut self.master, strip, n);
            }
        }

        let master = &mut self.master;
        master.run_processing_and_inserts(n, true);
        master.apply_gain(n);
        master
            .shared
            .meter
            .record(&master.left[..n], &master.right[..n]);
        if let Some(record) = &master.record {
            record.write(
                &master.left[..n],
                &master.right[..n],
                &mut self.record_scratch,
            );
        }

        // Matrices: the master and buses after their faders.
        for matrix in &mut self.matrices {
            for source in &mut matrix.sources {
                let (from_l, from_r) = match source.from {
                    MixFrom::Master => (&self.master.left, &self.master.right),
                    MixFrom::Bus(index) => match self.buses.get(index) {
                        Some(bus) => (&bus.strip.left, &bus.strip.right),
                        None => continue,
                    },
                    // Matrices never feed one another.
                    MixFrom::Matrix(_) => continue,
                };
                let target = source.gains.load();
                let start = source.current;
                source.current = target;
                if start == (0.0, 0.0) && target == (0.0, 0.0) {
                    continue;
                }
                let steps = n.max(1) as f32;
                let step = ((target.0 - start.0) / steps, (target.1 - start.1) / steps);
                let strip = &mut matrix.strip;
                for i in 0..n {
                    let at = (i + 1) as f32;
                    strip.left[i] += from_l[i] * (start.0 + step.0 * at);
                    strip.right[i] += from_r[i] * (start.1 + step.1 * at);
                }
            }
            matrix.strip.finish_mix(
                matrix.stereo,
                &mut self.monitor,
                &mut self.record_scratch,
                n,
            );
        }

        let source = mix_buffers(
            self.monitor.source,
            &self.master,
            &self.buses,
            &self.matrices,
            n,
        );
        let previous = self
            .monitor
            .fade_from
            .and_then(|from| mix_buffers(from, &self.master, &self.buses, &self.matrices, n));
        self.monitor.finish(
            (&self.master.left[..n], &self.master.right[..n]),
            source,
            previous,
            n,
        );

        for plane in &mut self.outputs {
            plane[..n].fill(0.0);
        }
        for patch in &self.patches {
            let (left, right) = match patch.from {
                PatchFrom::Master => (&self.master.left, &self.master.right),
                PatchFrom::Monitor => (&self.monitor.left, &self.monitor.right),
                PatchFrom::Bus(index) => match self.buses.get(index) {
                    Some(bus) => (&bus.strip.left, &bus.strip.right),
                    None => continue,
                },
                PatchFrom::Channel(index) => match self.channels.get(index) {
                    Some(channel) => (&channel.strip.left, &channel.strip.right),
                    None => continue,
                },
                PatchFrom::Matrix(index) => match self.matrices.get(index) {
                    Some(matrix) => (&matrix.strip.left, &matrix.strip.right),
                    None => continue,
                },
            };
            match patch.right {
                Some(out_right) => {
                    if let Some(plane) = self.outputs.get_mut(patch.left) {
                        add(&mut plane[..n], &left[..n], 1.0);
                    }
                    if let Some(plane) = self.outputs.get_mut(out_right) {
                        add(&mut plane[..n], &right[..n], 1.0);
                    }
                }
                None => {
                    if let Some(plane) = self.outputs.get_mut(patch.left) {
                        add(&mut plane[..n], &left[..n], 0.5);
                        add(&mut plane[..n], &right[..n], 0.5);
                    }
                }
            }
        }
    }
}

fn add(into: &mut [f32], from: &[f32], gain: f32) {
    for (a, b) in into.iter_mut().zip(from) {
        *a += *b * gain;
    }
}

fn mix_into(into: &mut RtStrip, from: &RtStrip, n: usize) {
    add(&mut into.left[..n], &from.left[..n], 1.0);
    add(&mut into.right[..n], &from.right[..n], 1.0);
}

/// Add `strip`'s current signal into each of its sends at `tap`, ramping
/// each send's per-side gains across the block.
fn send_into(sends: &mut [RtSend], tap: SendTap, strip: &RtStrip, buses: &mut [RtBus], n: usize) {
    for send in sends.iter_mut().filter(|s| s.tap == tap) {
        let target = send.gains.load();
        let start = send.current;
        send.current = target;
        let Some(bus) = buses.get_mut(send.bus) else {
            continue;
        };
        if start == (0.0, 0.0) && target == (0.0, 0.0) {
            continue;
        }
        let steps = n.max(1) as f32;
        let step = ((target.0 - start.0) / steps, (target.1 - start.1) / steps);
        for i in 0..n {
            let at = (i + 1) as f32;
            bus.strip.left[i] += strip.left[i] * (start.0 + step.0 * at);
            bus.strip.right[i] += strip.right[i] * (start.1 + step.1 * at);
        }
    }
}

/// Add a mono `signal` to both sides of each destination's sum.
fn inject(
    to: &[InjectTo],
    signal: &[f32],
    master: &mut RtStrip,
    buses: &mut [RtBus],
    matrices: &mut [RtMatrix],
    monitor: &mut RtMonitor,
) {
    let n = signal.len();
    for dest in to {
        let (left, right) = match *dest {
            InjectTo::Master => (&mut master.left, &mut master.right),
            InjectTo::Bus(i) => match buses.get_mut(i) {
                Some(bus) => (&mut bus.strip.left, &mut bus.strip.right),
                None => continue,
            },
            InjectTo::Matrix(i) => match matrices.get_mut(i) {
                Some(matrix) => (&mut matrix.strip.left, &mut matrix.strip.right),
                None => continue,
            },
            InjectTo::Monitor => (&mut monitor.left, &mut monitor.right),
        };
        add(&mut left[..n], signal, 1.0);
        add(&mut right[..n], signal, 1.0);
    }
}

/// A mix's post-fader buffers, if it exists.
fn mix_buffers<'a>(
    from: MixFrom,
    master: &'a RtStrip,
    buses: &'a [RtBus],
    matrices: &'a [RtMatrix],
    n: usize,
) -> Option<(&'a [f32], &'a [f32])> {
    let strip = match from {
        MixFrom::Master => master,
        MixFrom::Bus(i) => &buses.get(i)?.strip,
        MixFrom::Matrix(i) => &matrices.get(i)?.strip,
    };
    Some((&strip.left[..n], &strip.right[..n]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bypassing an insert fades the effect out over [`BYPASS_FADE_SECONDS`]
    /// rather than cutting it, and un-bypassing fades it back in.
    #[test]
    fn switching_the_bypass_fades_instead_of_cutting() {
        let sr = 48_000;
        let fx = BuiltinFx::new("equz8", sr).unwrap();
        let (cell, params) = InsertCell::new(InsertDsp::Builtin(fx), false, sr);
        let output_db = crate::builtin_fx::builtin_params("equz8")
            .into_iter()
            .find(|p| p.id == "outputDb")
            .unwrap()
            .index;
        params.send((output_db, -24.0)).unwrap();
        let block = 128;
        let mut out = Vec::new();
        let play = |cell: &InsertCell, blocks: usize, out: &mut Vec<f32>| {
            for _ in 0..blocks {
                let mut left = vec![0.5f32; block];
                let mut right = vec![0.5f32; block];
                cell.process(&mut left, &mut right);
                out.extend_from_slice(&left);
            }
        };
        play(&cell, 40, &mut out); // the -24 dB settles
        let quiet = *out.last().unwrap();
        assert!(quiet < 0.05, "{quiet}");
        let switched = out.len();
        cell.bypass.store(true, Ordering::Relaxed);
        play(&cell, 20, &mut out);
        // No step: each sample moves at most the whole swing over the fade.
        let fade = (BYPASS_FADE_SECONDS * sr as f32) as usize;
        let largest = out[switched - 1..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(largest <= (0.5 - quiet) / fade as f32 * 1.5, "{largest}");
        // Done after the fade: the input itself.
        assert!((out[switched + fade + 1] - 0.5).abs() < 1.0e-6);
        assert!((out[switched + fade / 2] - 0.5).abs() > 0.1);
        // And back in, as smoothly.
        let back = out.len();
        cell.bypass.store(false, Ordering::Relaxed);
        play(&cell, 20, &mut out);
        let largest = out[back - 1..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(largest <= (0.5 - quiet) / fade as f32 * 1.5, "{largest}");
        assert!((out.last().unwrap() - quiet).abs() < 1.0e-3);
    }

    fn strip(gain: f32) -> RtStrip {
        let shared = Arc::new(StripShared::default());
        shared.gain_l.store(gain);
        shared.gain_r.store(gain);
        let (processor, _) = ProcessorCell::new(48_000, &Processing::default());
        RtStrip::new(shared, processor, Vec::new(), None)
    }

    fn monitor() -> RtMonitor {
        RtMonitor::new(Arc::new(MonitorShared::default()))
    }

    /// Settings sent faster than the audio thread takes them: the newest
    /// always arrives, even with the queue full and nobody draining it.
    #[test]
    fn the_newest_processing_settings_win() {
        let (cell, sender) = ProcessorCell::new(48_000, &Processing::default());
        let mut last = Processing::default();
        for i in 0..(PROCESSING_QUEUE * 4) {
            last.delay.ms = i as f32;
            sender.send(last);
        }
        assert_eq!(cell.take_latest(), Some(last));
        assert_eq!(cell.take_latest(), None);
        let mut left = vec![0.1; 64];
        let mut right = left.clone();
        cell.process(&mut left, &mut right, false);
        let meters = cell.take_meters();
        assert!(meters.gate_db >= 0.0 && meters.comp_db >= 0.0);
    }

    /// Two inputs, each on a mono channel to the master; one channel also
    /// sent to a bus patched to its own outputs.
    #[test]
    fn channels_sum_to_master_and_sends_reach_their_bus_output() {
        let gains = Arc::new(SendGains::new(0.5, 0.5));
        let channels = vec![
            RtChannel {
                strip: strip(1.0),
                input_left: Some(0),
                input_right: None,
                sends: vec![RtSend::new(0, gains, SendTap::AfterFader)],
                dest: Dest::Master,
                record_input: true,
            },
            RtChannel {
                strip: strip(1.0),
                input_left: Some(1),
                input_right: None,
                sends: Vec::new(),
                dest: Dest::Master,
                record_input: true,
            },
        ];
        let buses = vec![RtBus {
            strip: strip(1.0),
            dest: Dest::None,
            stereo: true,
        }];
        let patches = vec![
            RtPatch {
                from: PatchFrom::Master,
                left: 0,
                right: Some(1),
            },
            RtPatch {
                from: PatchFrom::Bus(0),
                left: 2,
                right: Some(3),
            },
        ];
        let mut graph = Graph::new(2, 4, channels, buses, strip(1.0), monitor(), patches);
        let frames = 64;
        let input: Vec<f32> = (0..frames).flat_map(|_| [0.25, 0.5]).collect();
        let mut output = vec![0.0; frames * 4];
        graph.process(&input, &mut output);
        graph.process(&input, &mut output);
        let last = &output[(frames - 1) * 4..];
        assert!((last[0] - 0.75).abs() < 1e-6, "master L = both channels");
        assert!((last[1] - 0.75).abs() < 1e-6);
        assert!(
            (last[2] - 0.125).abs() < 1e-6,
            "the bus hears only the send"
        );
        assert!((last[3] - 0.125).abs() < 1e-6);
    }

    #[test]
    fn a_long_callback_is_processed_in_pieces() {
        let channels = vec![RtChannel {
            strip: strip(1.0),
            input_left: Some(0),
            input_right: None,
            sends: Vec::new(),
            dest: Dest::Master,
            record_input: true,
        }];
        let patches = vec![RtPatch {
            from: PatchFrom::Master,
            left: 0,
            right: None,
        }];
        let mut graph = Graph::new(1, 1, channels, Vec::new(), strip(1.0), monitor(), patches);
        let frames = MAX_BLOCK * 2 + 17;
        let input = vec![0.5; frames];
        let mut output = vec![0.0; frames];
        graph.process(&input, &mut output);
        assert!(output.iter().all(|s| (s - 0.5).abs() < 1e-6));
    }
}

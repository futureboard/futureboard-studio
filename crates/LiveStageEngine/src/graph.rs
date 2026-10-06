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
//! never resets a reverb tail or a compressor's envelope.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::builtin_fx::BuiltinFx;
use crate::ring::SampleRing;

/// Largest block the graph processes at once. A device callback larger than
/// this is processed in pieces.
pub const MAX_BLOCK: usize = 1024;

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
        // Non-negative floats order the same as their bit patterns, so an
        // integer max is a float max.
        self.peak_l
            .fetch_max(peak(left).to_bits(), Ordering::Relaxed);
        self.peak_r
            .fetch_max(peak(right).to_bits(), Ordering::Relaxed);
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
    /// Final per-side gain: fader × pan × (heard or not).
    pub gain_l: AtomicF32,
    pub gain_r: AtomicF32,
    /// Channel input gain (linear, sign carries the phase flip).
    pub trim: AtomicF32,
    pub meter: Meter,
    /// Level entering the strip, after trim — what the input meter shows.
    pub input_meter: Meter,
}

impl Default for StripShared {
    fn default() -> Self {
        Self {
            gain_l: AtomicF32::new(1.0),
            gain_r: AtomicF32::new(1.0),
            trim: AtomicF32::new(1.0),
            meter: Meter::default(),
            input_meter: Meter::default(),
        }
    }
}

/// The DSP of one insert.
pub enum InsertDsp {
    Builtin(BuiltinFx),
    #[cfg(feature = "external-plugins")]
    External(crate::external::ExternalInsert),
}

/// One insert's effect and its live controls. Shared by successive graphs.
pub struct InsertCell {
    dsp: UnsafeCell<InsertDsp>,
    pub bypass: AtomicBool,
    /// Wire parameter changes waiting for the next block.
    params: crossbeam_channel::Receiver<(u32, f32)>,
}

// SAFETY: `dsp` is touched only by the audio thread (exactly one graph is
// live there at a time), or by the control thread before the cell is first
// published. Everything else is atomics and a channel.
unsafe impl Sync for InsertCell {}
unsafe impl Send for InsertCell {}

impl InsertCell {
    /// The cell, and the sender its parameter changes go through.
    pub fn new(dsp: InsertDsp, bypass: bool) -> (Arc<Self>, crossbeam_channel::Sender<(u32, f32)>) {
        let (tx, rx) = crossbeam_channel::bounded(1024);
        (
            Arc::new(Self {
                dsp: UnsafeCell::new(dsp),
                bypass: AtomicBool::new(bypass),
                params: rx,
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
        if self.bypass.load(Ordering::Relaxed) {
            return;
        }
        match dsp {
            InsertDsp::Builtin(fx) => fx.process(left, right),
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
    pub inserts: Vec<Arc<InsertCell>>,
    pub record: Option<Arc<RecordStream>>,
    /// Gains applied at the end of the last block; ramped to the targets.
    current: (f32, f32),
    left: Vec<f32>,
    right: Vec<f32>,
}

impl RtStrip {
    pub fn new(
        shared: Arc<StripShared>,
        inserts: Vec<Arc<InsertCell>>,
        record: Option<Arc<RecordStream>>,
    ) -> Self {
        let current = (shared.gain_l.load(), shared.gain_r.load());
        Self {
            shared,
            inserts,
            record,
            current,
            left: vec![0.0; MAX_BLOCK],
            right: vec![0.0; MAX_BLOCK],
        }
    }

    fn run_inserts(&mut self, n: usize) {
        for insert in &self.inserts {
            insert.process(&mut self.left[..n], &mut self.right[..n]);
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
}

pub struct RtSend {
    pub bus: usize,
    pub gain: Arc<AtomicF32>,
    pub pre_fader: bool,
    current: f32,
}

impl RtSend {
    pub fn new(bus: usize, gain: Arc<AtomicF32>, pre_fader: bool) -> Self {
        let current = gain.load();
        Self {
            bus,
            gain,
            pre_fader,
            current,
        }
    }
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchFrom {
    Master,
    Bus(usize),
    Channel(usize),
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
    pub patches: Vec<RtPatch>,
    inputs: Vec<Vec<f32>>,
    outputs: Vec<Vec<f32>>,
    record_scratch: Vec<f32>,
}

impl Graph {
    pub fn new(
        in_channels: usize,
        out_channels: usize,
        channels: Vec<RtChannel>,
        buses: Vec<RtBus>,
        master: RtStrip,
        patches: Vec<RtPatch>,
    ) -> Self {
        Self {
            generation: 0,
            in_channels,
            out_channels,
            channels,
            buses,
            master,
            patches,
            inputs: (0..in_channels).map(|_| vec![0.0; MAX_BLOCK]).collect(),
            outputs: (0..out_channels).map(|_| vec![0.0; MAX_BLOCK]).collect(),
            record_scratch: vec![0.0; MAX_BLOCK * 2],
        }
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
        self.master.left[..n].fill(0.0);
        self.master.right[..n].fill(0.0);

        for channel in &mut self.channels {
            let strip = &mut channel.strip;
            let trim = strip.shared.trim.load();
            match (channel.input_left, channel.input_right) {
                (Some(l), right) if l < self.inputs.len() => {
                    let r = right.filter(|r| *r < self.inputs.len()).unwrap_or(l);
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
            strip.run_inserts(n);
            if !channel.record_input {
                if let Some(record) = &strip.record {
                    record.write(
                        &strip.left[..n],
                        &strip.right[..n],
                        &mut self.record_scratch,
                    );
                }
            }
            send_into(&mut channel.sends, true, strip, &mut self.buses, n);
            strip.apply_gain(n);
            strip
                .shared
                .meter
                .record(&strip.left[..n], &strip.right[..n]);
            send_into(&mut channel.sends, false, strip, &mut self.buses, n);
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
            strip.run_inserts(n);
            strip.apply_gain(n);
            strip
                .shared
                .meter
                .record(&strip.left[..n], &strip.right[..n]);
            if let Some(record) = &strip.record {
                record.write(
                    &strip.left[..n],
                    &strip.right[..n],
                    &mut self.record_scratch,
                );
            }
            if bus.dest == Dest::Master {
                mix_into(&mut self.master, strip, n);
            }
        }

        let master = &mut self.master;
        master.run_inserts(n);
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

        for plane in &mut self.outputs {
            plane[..n].fill(0.0);
        }
        for patch in &self.patches {
            let strip = match patch.from {
                PatchFrom::Master => &self.master,
                PatchFrom::Bus(index) => match self.buses.get(index) {
                    Some(bus) => &bus.strip,
                    None => continue,
                },
                PatchFrom::Channel(index) => match self.channels.get(index) {
                    Some(channel) => &channel.strip,
                    None => continue,
                },
            };
            match patch.right {
                Some(right) => {
                    if let Some(plane) = self.outputs.get_mut(patch.left) {
                        add(&mut plane[..n], &strip.left[..n], 1.0);
                    }
                    if let Some(plane) = self.outputs.get_mut(right) {
                        add(&mut plane[..n], &strip.right[..n], 1.0);
                    }
                }
                None => {
                    if let Some(plane) = self.outputs.get_mut(patch.left) {
                        add(&mut plane[..n], &strip.left[..n], 0.5);
                        add(&mut plane[..n], &strip.right[..n], 0.5);
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

/// Add `strip`'s current signal into each of its `pre_fader` sends' buses,
/// ramping each send's level across the block.
fn send_into(
    sends: &mut [RtSend],
    pre_fader: bool,
    strip: &RtStrip,
    buses: &mut [RtBus],
    n: usize,
) {
    for send in sends.iter_mut().filter(|s| s.pre_fader == pre_fader) {
        let target = send.gain.load();
        let start = send.current;
        send.current = target;
        let Some(bus) = buses.get_mut(send.bus) else {
            continue;
        };
        if start == 0.0 && target == 0.0 {
            continue;
        }
        let step = (target - start) / n.max(1) as f32;
        for i in 0..n {
            let gain = start + step * (i + 1) as f32;
            bus.strip.left[i] += strip.left[i] * gain;
            bus.strip.right[i] += strip.right[i] * gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(gain: f32) -> RtStrip {
        let shared = Arc::new(StripShared::default());
        shared.gain_l.store(gain);
        shared.gain_r.store(gain);
        RtStrip::new(shared, Vec::new(), None)
    }

    /// Two inputs, each on a mono channel to the master; one channel also
    /// sent to a bus patched to its own outputs.
    #[test]
    fn channels_sum_to_master_and_sends_reach_their_bus_output() {
        let gain = Arc::new(AtomicF32::new(0.5));
        let channels = vec![
            RtChannel {
                strip: strip(1.0),
                input_left: Some(0),
                input_right: None,
                sends: vec![RtSend::new(0, gain, false)],
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
        let mut graph = Graph::new(2, 4, channels, buses, strip(1.0), patches);
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
        let mut graph = Graph::new(1, 1, channels, Vec::new(), strip(1.0), patches);
        let frames = MAX_BLOCK * 2 + 17;
        let input = vec![0.5; frames];
        let mut output = vec![0.0; frames];
        graph.process(&input, &mut output);
        assert!(output.iter().all(|s| (s - 0.5).abs() < 1e-6));
    }
}

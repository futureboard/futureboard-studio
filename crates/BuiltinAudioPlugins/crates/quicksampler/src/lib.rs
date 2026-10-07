//! Quick Sampler — Futureboard's built-in one-sample instrument plug-in.
//!
//! One audio file played across the keyboard: pitched from a root key (or
//! unpitched, for a one-shot), from a start point to an end point, forwards or
//! reversed, once or looping, through an amp envelope, a filter, and the
//! instrument's level and pan.
//!
//! Runs in the plug-in host like every built-in: the host builds a [`Dsp`],
//! applies wire params from the shared ring ([`Dsp::apply_wire_param`]),
//! feeds it MIDI ([`Dsp::note_on`] and friends) and renders blocks
//! ([`Dsp::process_block`]). A decoded sample reaches the audio thread
//! through [`SampleLoader`]: a wait-free hand-off adopted at a block boundary,
//! with the sample it replaces handed back to be dropped off the audio thread.
//!
//! Its editor is native (GPUI), drawn by Studio; [`ui::UI_ORIGIN`] is the key
//! Studio's state mirror files it under.

mod engine;
mod handoff;
pub mod ipc;
mod params;
mod sample;
pub mod ui;

use std::sync::Arc;

use builtin_dsp_core::time_constant;
use serde::{Deserialize, Serialize};

pub use engine::{
    CONTROLLER_CHANNEL_PRESSURE, CONTROLLER_PITCH_BEND, CONTROLLER_PROGRAM_CHANGE, QuickSampler,
};
pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};
pub use params::{
    CUTOFF_MAX_HZ, CUTOFF_MIN_HZ, ENVELOPE_MAX_MS, FilterMode, LoopMode, MAX_POLYPHONY,
    PITCH_BEND_MAX, QuickSamplerParams, TRANSPOSE_RANGE,
};
pub use sample::{SampleData, load_cached};

use handoff::HandoffCell;

pub const PLUGIN_ID: &str = "futureboard.quicksampler";
pub const PLUGIN_NAME: &str = "Quick Sampler";

/// Frames rendered per inner chunk. A host block longer than this is rendered
/// in several chunks through the same preallocated scratch.
const CHUNK_FRAMES: usize = 1_024;
const METER_RELEASE_SEC: f32 = 0.3;
const RMS_SEC: f32 = 0.3;

/// Everything the plug-in persists: how it plays, and which file it plays.
/// The audio itself is not here — it is re-read from the plug-in's Samples
/// folder by name and sent again whenever the DSP (re)starts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Params {
    /// File name inside the plug-in's Samples folder. Persistence only.
    pub sample_name: Option<String>,
    pub sampler: QuickSamplerParams,
}

pub fn default_params() -> Params {
    Params::default()
}

/// Output levels, in the shape every metering built-in hands the host. An
/// instrument has no audio input, so the input side stays zero.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterFrame {
    pub out_peak: f32,
    pub out_rms: f32,
    pub out_clip: bool,
}

/// What travels through the hand-off cells: a sample, or `None` to unload.
/// Moved as a `Box` so the audio thread only ever swaps pointers.
struct SampleSlot {
    sample: Option<Arc<SampleData>>,
}

struct SampleChannel {
    /// Control → audio: the next sample to play.
    pending: HandoffCell<SampleSlot>,
    /// Audio → control: the sample it replaced, to be dropped off the audio
    /// thread.
    retired: HandoffCell<SampleSlot>,
}

/// Control-side handle that hands a decoded sample to the audio thread.
/// Cheap to clone; safe from the plug-in host's IPC thread.
#[derive(Clone)]
pub struct SampleLoader {
    channel: Arc<SampleChannel>,
}

impl SampleLoader {
    /// Queues `sample` (or, with `None`, silence) for the next block. A sample
    /// queued earlier and not yet adopted is replaced and dropped here.
    pub fn submit(&self, sample: Option<Arc<SampleData>>) {
        self.collect_garbage();
        drop(self.channel.pending.put(Box::new(SampleSlot { sample })));
    }

    /// Drops whatever the audio thread has handed back.
    pub fn collect_garbage(&self) {
        drop(self.channel.retired.take());
    }
}

pub struct Dsp {
    sampler: QuickSampler,
    params: Params,
    channel: Arc<SampleChannel>,
    /// The slot the audio thread swaps a pending sample through. Always
    /// empty between blocks; it exists so adopting a sample moves pointers
    /// and never allocates or frees on the audio thread.
    spare: Option<Box<SampleSlot>>,
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
    out_peak: f32,
    out_mean_square: f32,
    out_clip: bool,
    meter_release: f32,
    rms_coeff: f32,
}

impl std::fmt::Debug for Dsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dsp")
            .field("sampler", &self.sampler)
            .field("sample_name", &self.params.sample_name)
            .finish()
    }
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let params = default_params();
        Self {
            sampler: QuickSampler::new(sample_rate as u32, None, params.sampler),
            params,
            channel: Arc::new(SampleChannel {
                pending: HandoffCell::new(),
                retired: HandoffCell::new(),
            }),
            spare: None,
            scratch_l: vec![0.0; CHUNK_FRAMES],
            scratch_r: vec![0.0; CHUNK_FRAMES],
            out_peak: 0.0,
            out_mean_square: 0.0,
            out_clip: false,
            meter_release: time_constant(sample_rate, METER_RELEASE_SEC),
            rms_coeff: time_constant(sample_rate, RMS_SEC),
        }
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Restores persisted state. Control thread, before the DSP is published.
    pub fn set_params(&mut self, mut params: Params) {
        params.sampler = params.sampler.sanitized();
        self.sampler.set_params(params.sampler);
        self.params = params;
    }

    /// Applies one wire edit. Allocation-free: a field write and the
    /// sampler's derived constants.
    pub fn apply_wire_param(&mut self, index: u32, value: f32) -> bool {
        if !ipc::apply_wire_param(&mut self.params.sampler, index, value) {
            return false;
        }
        self.sampler.set_params(self.params.sampler);
        true
    }

    /// The control-side handle the host submits decoded samples through.
    pub fn sample_loader(&self) -> SampleLoader {
        SampleLoader {
            channel: self.channel.clone(),
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) {
        self.note_on_channel(0, note, velocity);
    }

    pub fn note_on_channel(&mut self, channel: u8, note: u8, velocity: u8) {
        self.sampler.note_on(channel, note, velocity);
    }

    /// Takes new playing params without touching the persisted sample name —
    /// for a plug-in that drives this one as its voice engine. Allocation-free.
    pub fn set_sampler_params(&mut self, sampler: QuickSamplerParams) {
        self.params.sampler = sampler.sanitized();
        self.sampler.set_params(self.params.sampler);
    }

    /// Plays `start..end` (fractions of the sample) as a slice: see
    /// [`QuickSampler::note_on_region`].
    pub fn note_on_region(&mut self, channel: u8, note: u8, velocity: u8, start: f32, end: f32) {
        self.sampler
            .note_on_region(channel, note, velocity, start, end);
    }

    /// Cuts every sounding voice quickly: see [`QuickSampler::choke`].
    pub fn choke(&mut self) {
        self.sampler.choke();
    }

    pub fn note_off(&mut self, note: u8) {
        self.note_off_channel(0, note);
    }

    pub fn note_off_channel(&mut self, channel: u8, note: u8) {
        self.sampler.note_off(channel, note);
    }

    pub fn control_change(&mut self, channel: u8, controller: u8, value: u8) {
        self.sampler.controller(channel, controller, value);
    }

    pub fn pitch_bend(&mut self, channel: u8, value: u16) {
        self.sampler.pitch_bend(channel, value);
    }

    pub fn all_notes_off(&mut self) {
        self.sampler.all_notes_off(false);
    }

    pub fn active_voice_count(&self) -> usize {
        self.sampler.active_voice_count()
    }

    /// The newest sounding voices: see [`QuickSampler::sounding`].
    pub fn sounding(&self, out: &mut [(u8, u64, f32)]) -> usize {
        self.sampler.sounding(out)
    }

    /// Adopts a sample submitted since the last block. Realtime-safe: the old
    /// sample goes back through `retired` in the same box the new one arrived
    /// in. While the control side has not yet collected the previous one, the
    /// new sample waits a block rather than freeing anything here.
    fn adopt_pending(&mut self) {
        if let Some(previous) = self.channel.retired.take() {
            // Not collected yet: put it back and try again next block.
            if let Some(displaced) = self.channel.retired.put(previous) {
                // Unreachable — only this thread puts into `retired` — but a
                // box must never be freed here, so park it in the spare.
                self.spare = Some(displaced);
            }
            return;
        }
        let Some(mut slot) = self.channel.pending.take() else {
            return;
        };
        self.sampler.swap_sample(&mut slot.sample);
        // `slot` now holds the sample that was playing.
        if let Some(displaced) = self.channel.retired.put(slot) {
            self.spare = Some(displaced);
        }
    }

    /// Renders `frames` interleaved stereo frames into `interleaved`.
    pub fn process_block(&mut self, interleaved: &mut [f32], frames: usize) {
        self.adopt_pending();
        let frames = frames.min(interleaved.len() / 2);
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(CHUNK_FRAMES);
            let (left, right) = (&mut self.scratch_l[..n], &mut self.scratch_r[..n]);
            self.sampler.render(left, right);
            for i in 0..n {
                let (l, r) = (left[i], right[i]);
                interleaved[(done + i) * 2] = l;
                interleaved[(done + i) * 2 + 1] = r;
                let peak = l.abs().max(r.abs());
                self.out_peak = if peak > self.out_peak {
                    peak
                } else {
                    self.out_peak * self.meter_release
                };
                let square = 0.5 * (l * l + r * r);
                self.out_mean_square = square + self.rms_coeff * (self.out_mean_square - square);
                if peak >= 1.0 {
                    self.out_clip = true;
                }
            }
            done += n;
        }
    }

    pub fn meter_frame(&self) -> MeterFrame {
        MeterFrame {
            out_peak: self.out_peak,
            out_rms: self.out_mean_square.max(0.0).sqrt(),
            out_clip: self.out_clip,
        }
    }

    /// No lookahead: a note sounds on the sample it lands on.
    pub fn latency_samples(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize) -> Arc<SampleData> {
        let samples: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        Arc::new(SampleData::from_interleaved(&samples, 1, 48_000).unwrap())
    }

    fn peak(buffer: &[f32]) -> f32 {
        buffer.iter().fold(0.0_f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn a_submitted_sample_plays_from_the_next_block() {
        let mut dsp = Dsp::new(48_000.0);
        let loader = dsp.sample_loader();
        let mut out = vec![0.0; 512];
        dsp.note_on(60, 127);
        dsp.process_block(&mut out, 256);
        assert_eq!(peak(&out), 0.0, "nothing loaded yet");

        loader.submit(Some(tone(48_000)));
        dsp.process_block(&mut out, 256);
        dsp.note_on(60, 127);
        dsp.process_block(&mut out, 256);
        assert!(peak(&out) > 0.1);
        assert!(dsp.meter_frame().out_peak > 0.1);
    }

    #[test]
    fn a_replaced_sample_comes_back_to_the_control_side() {
        let mut dsp = Dsp::new(48_000.0);
        let loader = dsp.sample_loader();
        let first = tone(1_000);
        let watch = Arc::downgrade(&first);
        loader.submit(Some(first));
        let mut out = vec![0.0; 64];
        dsp.process_block(&mut out, 32);
        loader.submit(Some(tone(2_000)));
        dsp.process_block(&mut out, 32);
        assert!(watch.upgrade().is_some(), "the audio thread never frees it");
        loader.collect_garbage();
        assert!(watch.upgrade().is_none(), "the control side does");
    }

    #[test]
    fn wire_edits_reach_the_voices() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.sample_loader().submit(Some(tone(48_000)));
        let mut out = vec![0.0; 512];
        dsp.process_block(&mut out, 1);
        let volume = ipc::ui_param_index("volume").unwrap();
        assert!(dsp.apply_wire_param(volume, 0.0));
        dsp.note_on(60, 127);
        dsp.process_block(&mut out, 256);
        assert_eq!(peak(&out), 0.0);
        assert_eq!(dsp.params().sampler.volume, 0.0);
    }

    #[test]
    fn a_long_host_block_renders_in_chunks() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.sample_loader().submit(Some(tone(48_000)));
        let mut out = vec![0.0; 2 * 3_000];
        dsp.process_block(&mut out, 1);
        dsp.note_on(60, 127);
        dsp.process_block(&mut out, 3_000);
        assert!(peak(&out[2 * 2_500..]) > 0.1, "past the first chunk");
    }
}

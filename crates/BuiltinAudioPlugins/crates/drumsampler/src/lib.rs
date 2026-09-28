//! Drum Sampler - a realtime-safe 16-pad one-shot drum instrument.
//!
//! Each pad plays a user-loaded sample on its own mapped MIDI note. Sample
//! bytes are decoded off the audio thread (native plugin host, IPC thread)
//! and handed to the audio thread through a wait-free single-slot cell, the
//! same shape rodharerist uses for impulse responses
//! (`rodharerist::dsp::handoff::HandoffCell`) - decode, resample or FFT work
//! never happens on the realtime path, only a pointer swap does.

pub mod handoff;
pub mod ipc;
pub mod ui;

use builtin_dsp_core::{Instrument, ParamDescriptor, PluginCategory, PluginDescriptor, clamp};
use handoff::HandoffCell;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub use ipc::{UI_PARAM_IDS, ui_param_id, ui_param_index};

pub const PLUGIN_ID: &str = "futureboard.drumsampler";
pub const PADS: usize = 16;
pub const MAX_VOICES: usize = 32;
/// Fixed fade applied when one pad chokes another in the same group - short
/// enough to read as an instant cutoff (closed hat stopping an open hat)
/// regardless of the choked pad's own `release_ms`.
const CHOKE_FADE_MS: f32 = 15.0;

/// One decoded one-shot sample, interleaved, normalized to -1.0..1.0.
#[derive(Debug)]
pub struct PadBuffer {
    pub samples: Box<[f32]>,
    pub channels: usize,
    pub frames: usize,
    pub sample_rate: f32,
}

/// Control-thread <-> audio-thread hand-off for one pad's sample buffer.
///
/// Thread contract mirrors `rodharerist`'s `IrLoader`: exactly one control
/// thread calls `submit`/`collect_garbage`; only the audio thread calls
/// `adopt_pending`, and only ever from [`Dsp::note_on`] or [`Dsp::reset`].
struct PadChannel {
    pending: HandoffCell<PadBuffer>,
    retired: HandoffCell<PadBuffer>,
}

impl PadChannel {
    fn new() -> Self {
        Self {
            pending: HandoffCell::new(),
            retired: HandoffCell::new(),
        }
    }
}

/// Cloneable control-side handle for loading a pad's sample.
#[derive(Clone)]
pub struct PadLoader {
    channel: Arc<PadChannel>,
}

impl PadLoader {
    /// Drop any buffer the audio thread has retired (safe: it was already
    /// unlinked from the live pad on the audio thread).
    pub fn collect_garbage(&self) {
        if let Some(dead) = self.channel.retired.take() {
            drop(dead);
        }
    }

    /// Submit a freshly decoded buffer for the audio thread to adopt the next
    /// time this pad is triggered. A not-yet-adopted buffer already waiting
    /// is dropped here (safe: the audio thread never touched it).
    pub fn submit(&self, buffer: Box<PadBuffer>) {
        self.collect_garbage();
        if let Some(bumped) = self.channel.pending.put(buffer) {
            drop(bumped);
        }
    }
}

struct PadRuntime {
    channel: Arc<PadChannel>,
    buffer: Option<Box<PadBuffer>>,
}

impl PadRuntime {
    fn new() -> Self {
        Self {
            channel: Arc::new(PadChannel::new()),
            buffer: None,
        }
    }

    fn loader(&self) -> PadLoader {
        PadLoader {
            channel: self.channel.clone(),
        }
    }

    /// Audio-thread only: adopt a pending buffer if one is waiting, retiring
    /// (never dropping in place) whatever was live before it.
    fn adopt_pending(&mut self) {
        if let Some(fresh) = self.channel.pending.take() {
            if let Some(old) = self.buffer.replace(fresh) {
                if let Some(bumped) = self.channel.retired.put(old) {
                    // The control thread never drained the previous retiree -
                    // drop it here. This only happens if two loads land
                    // before a single `collect_garbage`, which is not the
                    // realtime path.
                    drop(bumped);
                }
            }
        }
    }
}

/// `sample_name` is display/persistence-only — the audio referenced by a
/// project reload is not itself in this struct, only its file name in the
/// plugin's Samples sandbox (see [`PadLoader`]). It is never read on the
/// audio thread, only cloned on the control thread for `ui_values`/state
/// serialization, so a `Pad` is intentionally not `Copy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pad {
    pub note: u8,
    pub tune_semitones: f32,
    pub gain_db: f32,
    pub pan: f32,
    pub choke_group: u8,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub reverse: bool,
    pub muted: bool,
    pub solo: bool,
    #[serde(default)]
    pub sample_name: Option<String>,
}

fn default_pad(index: usize) -> Pad {
    Pad {
        note: 36 + index as u8,
        tune_semitones: 0.0,
        gain_db: 0.0,
        pan: 0.0,
        choke_group: 0,
        attack_ms: 1.0,
        release_ms: 60.0,
        reverse: false,
        muted: false,
        solo: false,
        sample_name: None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Params {
    pub pads: [Pad; PADS],
}

pub fn default_params() -> Params {
    Params {
        pads: std::array::from_fn(default_pad),
    }
}

pub fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        id: PLUGIN_ID,
        name: "Drum Sampler",
        vendor: "Futureboard",
        category: PluginCategory::Instrument,
        version: env!("CARGO_PKG_VERSION"),
        params: &[
            ParamDescriptor {
                id: "pad0Gain",
                name: "Pad 1 Gain",
                default_value: 0.0,
                min: -60.0,
                max: 12.0,
                unit: "dB",
            },
            ParamDescriptor {
                id: "pad0Tune",
                name: "Pad 1 Tune",
                default_value: 0.0,
                min: -24.0,
                max: 24.0,
                unit: "st",
            },
            ParamDescriptor {
                id: "pad0Pan",
                name: "Pad 1 Pan",
                default_value: 0.0,
                min: -1.0,
                max: 1.0,
                unit: "",
            },
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvStage {
    Attack,
    Body,
    Release,
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    active: bool,
    pad_index: usize,
    position: f64,
    step: f64,
    velocity: f32,
    age: u64,
    env: f32,
    stage: EnvStage,
    release_per_sample: f32,
}

impl Voice {
    const fn silent() -> Self {
        Self {
            active: false,
            pad_index: 0,
            position: 0.0,
            step: 1.0,
            velocity: 0.0,
            age: 0,
            env: 0.0,
            stage: EnvStage::Body,
            release_per_sample: 0.0,
        }
    }
}

pub struct Dsp {
    sample_rate: f32,
    params: Params,
    pads: [PadRuntime; PADS],
    voices: [Voice; MAX_VOICES],
    age: u64,
}

impl std::fmt::Debug for Dsp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dsp")
            .field("sample_rate", &self.sample_rate)
            .field("params", &self.params)
            .field(
                "active_voices",
                &self.voices.iter().filter(|voice| voice.active).count(),
            )
            .finish()
    }
}

impl Dsp {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: sample_rate.max(1.0),
            params: default_params(),
            pads: std::array::from_fn(|_| PadRuntime::new()),
            voices: [Voice::silent(); MAX_VOICES],
            age: 0,
        }
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn set_params(&mut self, mut params: Params) {
        ipc::sanitize_params(&mut params);
        self.params = params;
    }

    pub fn apply_wire_param(&mut self, index: u32, value: f32) -> bool {
        ipc::apply_wire_param(&mut self.params, index, value)
    }

    /// Control-side handle used by the plugin host to submit a decoded sample
    /// for `pad_index`. Returns `None` for an out-of-range pad.
    pub fn pad_loader(&self, pad_index: usize) -> Option<PadLoader> {
        self.pads.get(pad_index).map(PadRuntime::loader)
    }

    pub fn all_notes_off(&mut self) {
        let sample_rate = self.sample_rate;
        for voice in &mut self.voices {
            if voice.active {
                begin_release(sample_rate, voice, CHOKE_FADE_MS);
            }
        }
    }

    fn allocate_voice(&self) -> usize {
        self.voices
            .iter()
            .position(|voice| !voice.active)
            .unwrap_or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by(|(_, left), (_, right)| {
                        left.env
                            .partial_cmp(&right.env)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| left.age.cmp(&right.age))
                    })
                    .map_or(0, |(index, _)| index)
            })
    }

    fn pad_index_for_note(&self, note: u8) -> Option<usize> {
        self.params.pads.iter().position(|pad| pad.note == note)
    }

    #[inline]
    fn render_voice(pad: &Pad, buffer: &PadBuffer, voice: &mut Voice) -> (f32, f32) {
        let frame_index = voice.position.floor() as i64;
        let frac = (voice.position - voice.position.floor()) as f32;
        let read_frame = |frame: i64, channel: usize| -> f32 {
            let resolved = if pad.reverse {
                buffer.frames as i64 - 1 - frame
            } else {
                frame
            };
            if resolved < 0 || resolved as usize >= buffer.frames {
                return 0.0;
            }
            let channels = buffer.channels.max(1);
            let sample_channel = channel.min(channels - 1);
            buffer.samples[resolved as usize * channels + sample_channel]
        };
        let (mono_l0, mono_r0) = if buffer.channels >= 2 {
            (read_frame(frame_index, 0), read_frame(frame_index, 1))
        } else {
            let mono = read_frame(frame_index, 0);
            (mono, mono)
        };
        let (mono_l1, mono_r1) = if buffer.channels >= 2 {
            (
                read_frame(frame_index + 1, 0),
                read_frame(frame_index + 1, 1),
            )
        } else {
            let mono = read_frame(frame_index + 1, 0);
            (mono, mono)
        };
        let sample_l = mono_l0 + (mono_l1 - mono_l0) * frac;
        let sample_r = mono_r0 + (mono_r1 - mono_r0) * frac;

        voice.position += voice.step;
        let end_reached = voice.position < 0.0 || voice.position as usize + 1 >= buffer.frames;

        let env = match voice.stage {
            EnvStage::Attack => voice.env,
            EnvStage::Body => 1.0,
            EnvStage::Release => voice.env,
        };
        if end_reached {
            voice.active = false;
        }

        let gain = builtin_dsp_core::db_to_linear(pad.gain_db) * voice.velocity * env;
        let pan = clamp(pad.pan, -1.0, 1.0);
        let left_gain = (0.5 * (1.0 - pan)).sqrt();
        let right_gain = (0.5 * (1.0 + pan)).sqrt();
        (sample_l * gain * left_gain, sample_r * gain * right_gain)
    }

    #[inline]
    fn advance_envelope(pad: &Pad, sample_rate: f32, voice: &mut Voice) {
        match voice.stage {
            EnvStage::Attack => {
                voice.env += 1.0 / (pad.attack_ms * 0.001 * sample_rate).max(1.0);
                if voice.env >= 1.0 {
                    voice.env = 1.0;
                    voice.stage = EnvStage::Body;
                }
            }
            EnvStage::Body => voice.env = 1.0,
            EnvStage::Release => {
                voice.env -= voice.release_per_sample;
                if voice.env <= 0.0 {
                    voice.env = 0.0;
                    voice.active = false;
                }
            }
        }
    }
}

impl Instrument for Dsp {
    fn reset(&mut self) {
        self.voices = [Voice::silent(); MAX_VOICES];
        for pad in &mut self.pads {
            pad.adopt_pending();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
    }

    fn note_on(&mut self, note: u8, velocity: u8) {
        if velocity == 0 {
            self.note_off(note);
            return;
        }
        let Some(pad_index) = self.pad_index_for_note(note) else {
            return;
        };
        self.pads[pad_index].adopt_pending();
        if self.pads[pad_index].buffer.is_none() {
            return;
        }
        // Scalars only: `Pad` also carries `sample_name` (a `String`), which
        // must never be cloned on this thread.
        let choke_group = self.params.pads[pad_index].choke_group;
        let tune_semitones = self.params.pads[pad_index].tune_semitones;

        // Choke: cut every other active voice sharing this pad's nonzero group.
        if choke_group != 0 {
            let sample_rate = self.sample_rate;
            let pads = &self.params.pads;
            for voice in &mut self.voices {
                if voice.active
                    && voice.pad_index != pad_index
                    && pads[voice.pad_index].choke_group == choke_group
                {
                    begin_release(sample_rate, voice, CHOKE_FADE_MS);
                }
            }
        }

        self.age = self.age.wrapping_add(1);
        let index = self.allocate_voice();
        let buffer = self.pads[pad_index].buffer.as_ref().expect("checked above");
        let pitch_ratio = 2.0f64.powf(f64::from(tune_semitones) / 12.0);
        let rate_ratio = f64::from(buffer.sample_rate) / f64::from(self.sample_rate);
        self.voices[index] = Voice {
            active: true,
            pad_index,
            position: 0.0,
            step: pitch_ratio * rate_ratio,
            velocity: f32::from(velocity) / 127.0,
            age: self.age,
            env: 0.0,
            stage: EnvStage::Attack,
            release_per_sample: 0.0,
        };
    }

    fn note_off(&mut self, _note: u8) {
        // One-shots: releasing the MIDI key does not stop playback. Only a
        // choke (another pad in the same group) or reaching the end of the
        // sample ends a voice - see `note_on` and `render_voice`.
    }

    fn process_stereo(&mut self) -> (f32, f32) {
        let any_solo = self.params.pads.iter().any(|pad| pad.solo);
        let mut left = 0.0;
        let mut right = 0.0;
        for voice in &mut self.voices {
            if !voice.active {
                continue;
            }
            let pad = &self.params.pads[voice.pad_index];
            Self::advance_envelope(pad, self.sample_rate, voice);
            if !voice.active {
                continue;
            }
            let audible = if any_solo { pad.solo } else { !pad.muted };
            if !audible {
                continue;
            }
            let Some(buffer) = self.pads[voice.pad_index].buffer.as_ref() else {
                voice.active = false;
                continue;
            };
            let (l, r) = Self::render_voice(pad, buffer, voice);
            left += l;
            right += r;
        }
        (soft_clip(left), soft_clip(right))
    }
}

#[inline]
fn soft_clip(sample: f32) -> f32 {
    sample / (1.0 + sample.abs())
}

#[inline]
fn begin_release(sample_rate: f32, voice: &mut Voice, release_ms: f32) {
    voice.stage = EnvStage::Release;
    voice.release_per_sample = voice.env / (release_ms * 0.001 * sample_rate).max(1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_with_tone(frames: usize, sample_rate: f32) -> PadBuffer {
        let samples: Vec<f32> = (0..frames)
            .map(|i| (i as f32 / frames as f32 * std::f32::consts::TAU * 4.0).sin() * 0.5)
            .collect();
        PadBuffer {
            samples: samples.into_boxed_slice(),
            channels: 1,
            frames,
            sample_rate,
        }
    }

    #[test]
    fn silent_pad_produces_no_sound() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.note_on(36, 110);
        let (l, r) = dsp.process_stereo();
        assert_eq!((l, r), (0.0, 0.0));
    }

    #[test]
    fn loaded_pad_plays_and_finishes() {
        let mut dsp = Dsp::new(48_000.0);
        let loader = dsp.pad_loader(0).unwrap();
        loader.submit(Box::new(buffer_with_tone(4_800, 48_000.0)));
        dsp.note_on(36, 127);
        let mut peak = 0.0f32;
        for _ in 0..10_000 {
            let (l, r) = dsp.process_stereo();
            assert!(l.is_finite() && r.is_finite());
            peak = peak.max(l.abs()).max(r.abs());
        }
        assert!(peak > 0.01);
        let (l, r) = dsp.process_stereo();
        assert_eq!((l, r), (0.0, 0.0));
    }

    #[test]
    fn choke_group_cuts_other_pad() {
        let mut dsp = Dsp::new(48_000.0);
        dsp.pad_loader(0)
            .unwrap()
            .submit(Box::new(buffer_with_tone(48_000, 48_000.0)));
        dsp.pad_loader(1)
            .unwrap()
            .submit(Box::new(buffer_with_tone(48_000, 48_000.0)));
        let mut params = dsp.params().clone();
        params.pads[0].choke_group = 1;
        params.pads[1].choke_group = 1;
        dsp.set_params(params);

        dsp.note_on(36, 127);
        for _ in 0..100 {
            let _ = dsp.process_stereo();
        }
        dsp.note_on(37, 127);
        // Voice 0 should now be releasing (choked), not steady-state active.
        assert!(dsp.voices[0].active);
        assert_eq!(dsp.voices[0].stage, EnvStage::Release);
    }

    #[test]
    fn voice_pool_is_bounded() {
        let mut dsp = Dsp::new(48_000.0);
        for pad_index in 0..PADS {
            dsp.pad_loader(pad_index)
                .unwrap()
                .submit(Box::new(buffer_with_tone(48_000, 48_000.0)));
        }
        for _ in 0..(MAX_VOICES + 20) {
            dsp.note_on(36, 100);
        }
        assert_eq!(
            dsp.voices.iter().filter(|voice| voice.active).count(),
            MAX_VOICES
        );
    }
}

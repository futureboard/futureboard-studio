//! The voice engine: one sample played across the keyboard.
//!
//! A note plays the sample pitched from its root key (or unpitched, for a
//! one-shot), from a start point to an end point — forwards or reversed, once
//! or looping — through an amp envelope, an optional filter, and the
//! instrument's level and pan.
//!
//! Realtime contract: [`QuickSampler::render`], the note and controller
//! methods, [`QuickSampler::set_params`] and [`QuickSampler::swap_sample`]
//! allocate nothing, take no lock and do no I/O. The voice pool is allocated
//! once, at [`MAX_POLYPHONY`], when the sampler is built on a control thread;
//! a sample is decoded there too and shared immutably.

use std::sync::Arc;

use crate::params::{FilterMode, LoopMode, MAX_POLYPHONY, QuickSamplerParams};
use crate::sample::SampleData;

/// The engine's out-of-band controller numbers (see the Soundfont Player):
/// controller lanes above the MIDI CC range that carry pitch bend, channel
/// pressure and program change.
pub const CONTROLLER_CHANNEL_PRESSURE: u8 = 128;
pub const CONTROLLER_PITCH_BEND: u8 = 129;
pub const CONTROLLER_PROGRAM_CHANGE: u8 = 130;

/// Shortest attack and release: long enough to take the click off a start
/// point or a note-off in the middle of a waveform, too short to soften a
/// drum hit audibly.
const MIN_RAMP_MS: f32 = 0.2;
/// Fade given to a voice that is taken over by a newer note.
const STEAL_MS: f32 = 3.0;
/// Fade at the end of a sample that does not loop, so an end point inside the
/// waveform does not click.
const END_DECLICK_MS: f32 = 2.0;
/// Loop shorter than this, in frames, is ignored: a zero-length loop would
/// spin on one sample.
const MIN_LOOP_FRAMES: f64 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Debug, Clone, Copy)]
struct Voice {
    stage: Stage,
    level: f32,
    release_step: f32,
    channel: u8,
    note: u8,
    /// The key is still down.
    held: bool,
    /// Released while the sustain pedal was down; ends when the pedal lifts.
    sustained: bool,
    /// Fading out to make room for a newer note.
    stolen: bool,
    velocity_gain: f32,
    /// Read position, in sample frames.
    pos: f64,
    /// The frames this voice plays: the sampler's region, or a slice.
    start: f64,
    end: f64,
    /// Follows the sampler's loop. A slice never loops.
    looping: bool,
    /// +1 forwards, -1 backwards.
    dir: f64,
    /// Pitch offset from the sample's own pitch, before pitch bend.
    semitones: f32,
    age: u64,
    ic1: [f32; 2],
    ic2: [f32; 2],
}

impl Voice {
    const IDLE: Voice = Voice {
        stage: Stage::Idle,
        level: 0.0,
        release_step: 0.0,
        channel: 0,
        note: 0,
        held: false,
        sustained: false,
        stolen: false,
        velocity_gain: 1.0,
        pos: 0.0,
        start: 0.0,
        end: 0.0,
        looping: false,
        dir: 1.0,
        semitones: 0.0,
        age: 0,
        ic1: [0.0; 2],
        ic2: [0.0; 2],
    };

    fn active(&self) -> bool {
        self.stage != Stage::Idle
    }
}

/// Per-sample constants derived from the params, the sample and the output
/// rate. Recomputed whenever any of them changes, never per sample.
#[derive(Debug, Clone, Copy, Default)]
struct Derived {
    /// Sample frames per output sample at the sample's own pitch.
    rate_ratio: f64,
    region_start: f64,
    region_end: f64,
    loop_on: bool,
    pingpong: bool,
    loop_start: f64,
    loop_end: f64,
    attack_step: f32,
    decay_step: f32,
    release_samples: f32,
    steal_samples: f32,
    declick_samples: f64,
    gain_l: f32,
    gain_r: f32,
    filter: FilterMode,
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
}

pub struct QuickSampler {
    sample: Option<Arc<SampleData>>,
    params: QuickSamplerParams,
    output_rate: f32,
    voices: Box<[Voice]>,
    clock: u64,
    bend: [f32; 16],
    pedal: [bool; 16],
    derived: Derived,
}

impl std::fmt::Debug for QuickSampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickSampler")
            .field("frames", &self.sample.as_ref().map(|s| s.frames()))
            .field("params", &self.params)
            .field("output_rate", &self.output_rate)
            .finish()
    }
}

impl QuickSampler {
    /// A sampler at `output_rate` playing `sample` (silent without one).
    /// Allocates the voice pool; control thread only.
    pub fn new(
        output_rate: u32,
        sample: Option<Arc<SampleData>>,
        params: QuickSamplerParams,
    ) -> Self {
        let mut sampler = Self {
            sample,
            params: params.sanitized(),
            output_rate: output_rate.max(1) as f32,
            voices: vec![Voice::IDLE; MAX_POLYPHONY as usize].into_boxed_slice(),
            clock: 0,
            bend: [0.0; 16],
            pedal: [false; 16],
            derived: Derived::default(),
        };
        sampler.derive();
        sampler
    }

    pub fn sample(&self) -> Option<&Arc<SampleData>> {
        self.sample.as_ref()
    }

    pub fn params(&self) -> QuickSamplerParams {
        self.params
    }

    pub fn output_rate(&self) -> u32 {
        self.output_rate as u32
    }

    /// Takes new params. Sounding notes carry on with them: a new filter or
    /// level is heard at once, a new start or end point from the next note.
    pub fn set_params(&mut self, params: QuickSamplerParams) {
        let params = params.sanitized();
        if params != self.params {
            self.params = params;
            self.derive();
        }
    }

    fn derive(&mut self) {
        let p = self.params;
        let sr = self.output_rate;
        let samples = |ms: f32| (ms.max(MIN_RAMP_MS) * 0.001 * sr).max(1.0);
        let mut d = Derived {
            attack_step: 1.0 / samples(p.attack_ms),
            decay_step: if p.decay_ms <= 0.0 {
                1.0
            } else {
                (1.0 - p.sustain) / (p.decay_ms * 0.001 * sr).max(1.0)
            },
            release_samples: samples(p.release_ms),
            steal_samples: samples(STEAL_MS),
            declick_samples: (END_DECLICK_MS * 0.001 * sr) as f64,
            filter: p.filter,
            ..Derived::default()
        };
        let normalize = match &self.sample {
            Some(sample) if p.normalize && sample.peak() > 1.0e-6 => {
                (1.0 / sample.peak()).min(64.0)
            }
            _ => 1.0,
        };
        let level = p.volume * normalize;
        d.gain_l = level * (1.0 - p.pan).min(1.0);
        d.gain_r = level * (1.0 + p.pan).min(1.0);

        if let Some(sample) = &self.sample {
            let frames = sample.frames() as f64;
            d.rate_ratio = sample.sample_rate() as f64 / sr as f64;
            d.region_start = (p.start as f64 * frames).floor();
            d.region_end = (p.end as f64 * frames).ceil().min(frames);
            d.loop_start = (p.loop_start as f64 * frames)
                .floor()
                .clamp(d.region_start, d.region_end);
            d.loop_end = (p.loop_end as f64 * frames)
                .ceil()
                .clamp(d.region_start, d.region_end);
            d.loop_on =
                p.loop_mode != LoopMode::Off && d.loop_end - d.loop_start >= MIN_LOOP_FRAMES;
            d.pingpong = p.loop_mode == LoopMode::PingPong;
        }

        // Topology-preserving state-variable filter (Simper). The cutoff stays
        // under Nyquist at any output rate.
        let cutoff = p.cutoff_hz.min(sr * 0.45);
        let g = (std::f32::consts::PI * cutoff / sr).tan();
        let k = 2.0 - 1.94 * p.resonance;
        d.k = k;
        d.a1 = 1.0 / (1.0 + g * (g + k));
        d.a2 = g * d.a1;
        d.a3 = g * d.a2;
        self.derived = d;
    }

    /// Starts `note`. Velocity 0 is a note-off, as in MIDI.
    pub fn note_on(&mut self, channel: u8, note: u8, velocity: u8) {
        if velocity == 0 {
            self.note_off(channel, note);
            return;
        }
        let (d, p) = (self.derived, self.params);
        let keyed = if p.keytrack {
            note as f32 - p.root_note as f32
        } else {
            0.0
        };
        self.start_voice(
            channel,
            note,
            velocity,
            (d.region_start, d.region_end),
            keyed,
            true,
        );
    }

    /// Starts `note` playing `start..end` (fractions of the sample) at the
    /// sample's own pitch, moved only by transpose, fine tune and pitch bend:
    /// a slice. It never loops. Velocity 0 is a note-off.
    pub fn note_on_region(&mut self, channel: u8, note: u8, velocity: u8, start: f32, end: f32) {
        if velocity == 0 {
            self.note_off(channel, note);
            return;
        }
        let Some(frames) = self.sample.as_ref().map(|s| s.frames() as f64) else {
            return;
        };
        let from = (start.clamp(0.0, 1.0) as f64 * frames).floor();
        let to = (end.clamp(0.0, 1.0) as f64 * frames).ceil().min(frames);
        self.start_voice(channel, note, velocity, (from, to), 0.0, false);
    }

    /// Cuts every sounding voice in a few milliseconds, so the next note
    /// takes over cleanly: a choke group of one.
    pub fn choke(&mut self) {
        let steal = self.derived.steal_samples;
        for voice in self.voices.iter_mut().filter(|v| v.active() && !v.stolen) {
            voice.held = false;
            voice.sustained = false;
            voice.stolen = true;
            voice.stage = Stage::Release;
            voice.release_step = voice.level / steal;
        }
    }

    /// Starts a voice on `region` (frames), `keyed` semitones from the
    /// sample's pitch before transpose and fine tune.
    fn start_voice(
        &mut self,
        channel: u8,
        note: u8,
        velocity: u8,
        (start, end): (f64, f64),
        keyed: f32,
        looping: bool,
    ) {
        let channel = channel & 0x0F;
        let d = self.derived;
        if self.sample.is_none() || end - start < 1.0 {
            return;
        }
        let p = self.params;

        // Make room: past the polyphony, the oldest notes fade out — the ones
        // already releasing first.
        let sounding = self
            .voices
            .iter()
            .filter(|v| v.active() && !v.stolen)
            .count();
        for _ in 0..sounding.saturating_sub(p.polyphony as usize - 1) {
            let oldest = self
                .voices
                .iter()
                .enumerate()
                .filter(|(_, v)| v.active() && !v.stolen)
                .min_by_key(|(_, v)| (v.stage != Stage::Release, v.age))
                .map(|(index, _)| index);
            if let Some(index) = oldest {
                let voice = &mut self.voices[index];
                voice.stolen = true;
                voice.stage = Stage::Release;
                voice.release_step = voice.level / d.steal_samples;
            }
        }
        let slot = self
            .voices
            .iter()
            .position(|v| !v.active())
            .or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, v)| (!v.stolen, v.age))
                    .map(|(index, _)| index)
            })
            .unwrap_or(0);

        self.clock = self.clock.wrapping_add(1);
        let velocity = velocity.min(127) as f32 / 127.0;
        self.voices[slot] = Voice {
            stage: Stage::Attack,
            level: 0.0,
            release_step: 0.0,
            channel,
            note,
            held: true,
            sustained: false,
            stolen: false,
            velocity_gain: 1.0 - p.velocity + p.velocity * velocity,
            pos: if p.reverse { end - 1.0 } else { start },
            start,
            end,
            looping,
            dir: if p.reverse { -1.0 } else { 1.0 },
            semitones: keyed + p.transpose as f32 + p.fine_cents / 100.0,
            age: self.clock,
            ic1: [0.0; 2],
            ic2: [0.0; 2],
        };
    }

    pub fn note_off(&mut self, channel: u8, note: u8) {
        let channel = channel & 0x0F;
        let pedal = self.pedal[channel as usize];
        let release_samples = self.derived.release_samples;
        for voice in self.voices.iter_mut() {
            if voice.active() && voice.held && voice.channel == channel && voice.note == note {
                voice.held = false;
                if pedal {
                    voice.sustained = true;
                } else {
                    release(voice, release_samples);
                }
            }
        }
    }

    /// Releases every note. `immediate` fades them out in a few milliseconds
    /// instead of through the release.
    pub fn all_notes_off(&mut self, immediate: bool) {
        let steal = self.derived.steal_samples;
        let release_samples = self.derived.release_samples;
        for voice in self.voices.iter_mut().filter(|v| v.active()) {
            voice.held = false;
            voice.sustained = false;
            if immediate {
                voice.stolen = true;
                voice.stage = Stage::Release;
                voice.release_step = voice.level / steal;
            } else {
                release(voice, release_samples);
            }
        }
        self.pedal = [false; 16];
    }

    /// One controller-lane value: the engine's pitch-bend number, or a MIDI
    /// CC. Sustain (64), all sound off (120), reset controllers (121) and all
    /// notes off (123) are honoured; other controllers have nothing to drive.
    pub fn controller(&mut self, channel: u8, controller: u8, value: u8) {
        let channel = (channel & 0x0F) as usize;
        let value = value.min(127);
        match controller {
            CONTROLLER_PITCH_BEND => {
                let amount = ((value as f32 - 64.0) / 64.0).clamp(-1.0, 1.0);
                self.bend[channel] = amount * self.params.pitch_bend_range as f32;
            }
            64 => {
                let down = value >= 64;
                if self.pedal[channel] && !down {
                    let release_samples = self.derived.release_samples;
                    for voice in self.voices.iter_mut() {
                        if voice.active() && voice.sustained && voice.channel as usize == channel {
                            voice.sustained = false;
                            release(voice, release_samples);
                        }
                    }
                }
                self.pedal[channel] = down;
            }
            120 => self.all_notes_off(true),
            121 => {
                self.bend[channel] = 0.0;
            }
            123 => self.all_notes_off(false),
            _ => {}
        }
    }

    /// Exchanges the sampler's sample with `other`, silencing every voice —
    /// they were reading the old audio. Realtime-safe: the `Arc`s change
    /// places, nothing is allocated or freed here; the caller hands the old
    /// one to a control thread to drop.
    pub fn swap_sample(&mut self, other: &mut Option<Arc<SampleData>>) {
        std::mem::swap(&mut self.sample, other);
        for voice in self.voices.iter_mut() {
            *voice = Voice::IDLE;
        }
        self.derive();
    }

    /// A 14-bit MIDI pitch bend (`0x2000` is centre) on `channel`.
    pub fn pitch_bend(&mut self, channel: u8, value: u16) {
        let amount = ((value.min(0x3FFF) as f32 - 8192.0) / 8192.0).clamp(-1.0, 1.0);
        self.bend[(channel & 0x0F) as usize] = amount * self.params.pitch_bend_range as f32;
    }

    pub fn active_voice_count(&self) -> usize {
        self.voices.iter().filter(|v| v.active()).count()
    }

    /// The newest sounding voices, newest first, as `(note, start number,
    /// position)` — the start number tells a retrigger from a held note, the
    /// position is a fraction of the sample. Voices fading out under a newer
    /// note are left out. Fills `out`, returns how many it filled.
    /// Realtime-safe: no allocation.
    pub fn sounding(&self, out: &mut [(u8, u64, f32)]) -> usize {
        let frames = self.sample.as_ref().map_or(0.0, |s| s.frames() as f64);
        if frames <= 0.0 {
            return 0;
        }
        let mut count = 0;
        for voice in self.voices.iter().filter(|v| v.active() && !v.stolen) {
            let entry = (
                voice.note,
                voice.age,
                (voice.pos / frames).clamp(0.0, 1.0) as f32,
            );
            let mut at = count;
            while at > 0 && out[at - 1].1 < entry.1 {
                at -= 1;
            }
            if at >= out.len() {
                continue;
            }
            // Make room, dropping the oldest when full.
            let end = count.min(out.len() - 1);
            for i in (at..end).rev() {
                out[i + 1] = out[i];
            }
            out[at] = entry;
            count = (count + 1).min(out.len());
        }
        count
    }

    /// Renders `left.len()` frames, replacing what the buffers held. Lengths
    /// must match; the shorter one wins otherwise.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len().min(right.len());
        left[..frames].fill(0.0);
        right[..frames].fill(0.0);
        let Some(sample) = self.sample.as_deref() else {
            return;
        };
        let d = self.derived;
        let sustain = self.params.sustain;
        let (plane_l, plane_r) = (sample.left(), sample.right());
        let last = sample.frames().saturating_sub(1);
        for voice in self.voices.iter_mut().filter(|v| v.active()) {
            let semitones = voice.semitones + self.bend[voice.channel as usize];
            let step = d.rate_ratio * 2.0_f64.powf(semitones as f64 / 12.0);
            let looping = d.loop_on && voice.looping;
            for i in 0..frames {
                // Envelope.
                match voice.stage {
                    Stage::Attack => {
                        voice.level += d.attack_step;
                        if voice.level >= 1.0 {
                            voice.level = 1.0;
                            voice.stage = Stage::Decay;
                        }
                    }
                    Stage::Decay => {
                        voice.level -= d.decay_step;
                        if voice.level <= sustain {
                            voice.level = sustain;
                            voice.stage = Stage::Sustain;
                        }
                    }
                    Stage::Sustain => {
                        voice.level = sustain;
                        if sustain <= 0.0 {
                            voice.stage = Stage::Idle;
                            break;
                        }
                    }
                    Stage::Release => {
                        voice.level -= voice.release_step;
                        if voice.level <= 0.0 {
                            voice.stage = Stage::Idle;
                            break;
                        }
                    }
                    Stage::Idle => break,
                }

                let (mut l, mut r) = (
                    hermite(plane_l, voice.pos, last),
                    hermite(plane_r, voice.pos, last),
                );
                if d.filter != FilterMode::Off {
                    l = svf(&d, &mut voice.ic1[0], &mut voice.ic2[0], l);
                    r = svf(&d, &mut voice.ic1[1], &mut voice.ic2[1], r);
                }
                let mut gain = voice.level * voice.velocity_gain;
                if !looping {
                    let remaining = if voice.dir > 0.0 {
                        voice.end - voice.pos
                    } else {
                        voice.pos - voice.start
                    } / step;
                    if remaining < d.declick_samples {
                        gain *= (remaining / d.declick_samples.max(1.0)).max(0.0) as f32;
                    }
                }
                left[i] += l * gain * d.gain_l;
                right[i] += r * gain * d.gain_r;

                // Advance.
                voice.pos += step * voice.dir;
                if looping {
                    wrap_loop(voice, &d);
                } else if voice.pos >= voice.end || voice.pos < voice.start {
                    voice.stage = Stage::Idle;
                    break;
                }
            }
        }
    }
}

fn release(voice: &mut Voice, release_samples: f32) {
    if voice.stage != Stage::Idle && voice.stage != Stage::Release {
        voice.stage = Stage::Release;
        voice.release_step = voice.level / release_samples;
    }
}

/// Keeps a looping voice inside its loop: wrapping round, or bouncing off the
/// loop points for ping-pong. A voice that has not reached the loop yet (a
/// start point before it) plays on into it.
#[inline]
fn wrap_loop(voice: &mut Voice, d: &Derived) {
    let length = d.loop_end - d.loop_start;
    if d.pingpong {
        // A step can be longer than the loop at extreme transpositions; a
        // few reflections settle it, and a clamp settles anything left.
        for _ in 0..4 {
            if voice.dir > 0.0 && voice.pos >= d.loop_end {
                voice.pos = 2.0 * d.loop_end - voice.pos - 1.0;
                voice.dir = -1.0;
            } else if voice.dir < 0.0 && voice.pos < d.loop_start {
                voice.pos = 2.0 * d.loop_start - voice.pos;
                voice.dir = 1.0;
            } else {
                return;
            }
        }
        voice.pos = voice.pos.clamp(d.loop_start, d.loop_end - 1.0);
    } else if voice.dir > 0.0 && voice.pos >= d.loop_end {
        voice.pos = d.loop_start + (voice.pos - d.loop_start) % length;
    } else if voice.dir < 0.0 && voice.pos < d.loop_start {
        voice.pos = d.loop_end - (d.loop_start - voice.pos) % length;
    }
}

/// Four-point cubic Hermite read at fractional frame `pos`, with the
/// neighbours clamped to the sample.
#[inline]
fn hermite(plane: &[f32], pos: f64, last: usize) -> f32 {
    let base = pos.floor();
    let t = (pos - base) as f32;
    let i = base as isize;
    let at = |offset: isize| plane[(i + offset).clamp(0, last as isize) as usize];
    let (x0, x1, x2, x3) = (at(-1), at(0), at(1), at(2));
    let c1 = 0.5 * (x2 - x0);
    let c2 = x0 - 2.5 * x1 + 2.0 * x2 - 0.5 * x3;
    let c3 = 0.5 * (x3 - x0) + 1.5 * (x1 - x2);
    ((c3 * t + c2) * t + c1) * t + x1
}

#[inline]
fn svf(d: &Derived, ic1: &mut f32, ic2: &mut f32, input: f32) -> f32 {
    let v3 = input - *ic2;
    let v1 = d.a1 * *ic1 + d.a2 * v3;
    let v2 = *ic2 + d.a2 * *ic1 + d.a3 * v3;
    *ic1 = 2.0 * v1 - *ic1;
    *ic2 = 2.0 * v2 - *ic2;
    match d.filter {
        FilterMode::LowPass => v2,
        FilterMode::BandPass => v1,
        FilterMode::HighPass => input - d.k * v1 - v2,
        FilterMode::Off => input,
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;

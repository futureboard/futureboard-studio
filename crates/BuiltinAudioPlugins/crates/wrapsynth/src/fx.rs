//! WrapSynth's effects, after the voices: distortion, chorus, delay and
//! reverb, in that order, each switched on its own.
//!
//! The delay and the reverb are the built-in EchoSpace and VerbSpace cores,
//! driven from the synth's own handful of controls; the distortion and the
//! chorus are small enough to live here. Every buffer is sized when the rack
//! is built: the audio path only reads and writes them.

use builtin_dsp_core::StereoEffect;

use crate::{Params, soft_clip};

/// Centre and sweep of the chorus delay, in milliseconds.
const CHORUS_BASE_MS: f32 = 12.0;
const CHORUS_SWEEP_MS: f32 = 8.0;
/// Room the chorus lines leave above the deepest sweep.
const CHORUS_MAX_MS: f32 = 30.0;

struct Chorus {
    left: Box<[f32]>,
    right: Box<[f32]>,
    write: usize,
    phase: f32,
}

impl Chorus {
    fn new(sample_rate: f32) -> Self {
        let len = (sample_rate * CHORUS_MAX_MS * 0.001).ceil() as usize + 4;
        Self {
            left: vec![0.0; len].into_boxed_slice(),
            right: vec![0.0; len].into_boxed_slice(),
            write: 0,
            phase: 0.0,
        }
    }

    fn clear(&mut self) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        self.write = 0;
    }

    #[inline]
    fn read(buffer: &[f32], write: usize, delay: f32) -> f32 {
        let len = buffer.len();
        let position = write as f32 - delay;
        let position = if position < 0.0 {
            position + len as f32
        } else {
            position
        };
        let index = position.floor() as usize % len;
        let next = (index + 1) % len;
        let fraction = position - position.floor();
        buffer[index] + (buffer[next] - buffer[index]) * fraction
    }

    /// Two taps swept a quarter cycle apart, one per side.
    #[inline]
    fn process(&mut self, p: &Params, sample_rate: f32, left: f32, right: f32) -> (f32, f32) {
        let len = self.left.len();
        self.left[self.write] = left;
        self.right[self.write] = right;
        self.phase = (self.phase + p.chorus_rate_hz / sample_rate).fract();
        let ms_per_sample = sample_rate * 0.001;
        let sweep = |phase: f32| {
            let lfo = 0.5 + 0.5 * (std::f32::consts::TAU * phase).sin();
            (CHORUS_BASE_MS + p.chorus_depth * CHORUS_SWEEP_MS * lfo) * ms_per_sample
        };
        let wet_l = Self::read(&self.left, self.write, sweep(self.phase));
        let wet_r = Self::read(&self.right, self.write, sweep((self.phase + 0.25).fract()));
        self.write = (self.write + 1) % len;
        (
            left + (wet_l - left) * p.chorus_mix,
            right + (wet_r - right) * p.chorus_mix,
        )
    }
}

pub struct FxRack {
    sample_rate: f32,
    chorus: Chorus,
    delay: echospace::Dsp,
    reverb: verbspace::Dsp,
    /// What `configure` last switched on, to clear a tail that went stale
    /// while it was off.
    chorus_on: bool,
    delay_on: bool,
    reverb_on: bool,
}

impl std::fmt::Debug for FxRack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FxRack")
            .field("chorus_on", &self.chorus_on)
            .field("delay_on", &self.delay_on)
            .field("reverb_on", &self.reverb_on)
            .finish()
    }
}

impl FxRack {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let mut rack = Self {
            sample_rate,
            chorus: Chorus::new(sample_rate),
            delay: echospace::Dsp::new(sample_rate),
            reverb: verbspace::Dsp::new(sample_rate),
            chorus_on: false,
            delay_on: false,
            reverb_on: false,
        };
        rack.configure(&crate::default_params());
        rack
    }

    /// Rebuilds the buffers for a new rate. Not for the audio path.
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        let sample_rate = sample_rate.max(1.0);
        if (sample_rate - self.sample_rate).abs() < f32::EPSILON {
            return;
        }
        self.sample_rate = sample_rate;
        self.chorus = Chorus::new(sample_rate);
        self.delay.set_sample_rate(sample_rate);
        self.reverb.set_sample_rate(sample_rate);
    }

    pub fn set_tempo_bpm(&mut self, tempo_bpm: f32) {
        self.delay.set_tempo_bpm(tempo_bpm);
    }

    pub fn reset(&mut self) {
        self.chorus.clear();
        self.delay.reset();
        self.reverb.reset();
    }

    /// Carries the synth's effect controls into the delay and reverb cores.
    /// Allocation-free: their params are plain values. An effect switched on
    /// starts from silence, not from what it held when it was switched off.
    pub fn configure(&mut self, p: &Params) {
        let mut delay = echospace::default_params();
        delay.power = p.delay_on;
        delay.mode = if p.delay_ping_pong {
            echospace::DelayMode::PingPong
        } else {
            echospace::DelayMode::Stereo
        };
        delay.time_ms_l = p.delay_time_ms;
        delay.time_ms_r = p.delay_time_ms;
        delay.sync = p.delay_sync;
        delay.division_l = p.delay_division;
        delay.division_r = p.delay_division;
        delay.link = true;
        delay.feedback = p.delay_feedback * 100.0;
        delay.saturation = 0.0;
        delay.mix = p.delay_mix * 100.0;
        self.delay.set_params(delay);

        let mut reverb = verbspace::default_params();
        reverb.power = p.reverb_on;
        reverb.size = 10.0 + p.reverb_size * 90.0;
        reverb.decay_sec = p.reverb_decay_sec;
        reverb.damping = p.reverb_damping * 100.0;
        reverb.mix = p.reverb_mix * 100.0;
        self.reverb.set_params(reverb);

        if p.chorus_on && !self.chorus_on {
            self.chorus.clear();
        }
        if p.delay_on && !self.delay_on {
            self.delay.reset();
        }
        if p.reverb_on && !self.reverb_on {
            self.reverb.reset();
        }
        self.chorus_on = p.chorus_on;
        self.delay_on = p.delay_on;
        self.reverb_on = p.reverb_on;
    }

    #[inline]
    pub fn process(&mut self, p: &Params, mut left: f32, mut right: f32) -> (f32, f32) {
        if p.dist_on {
            let gain = 1.0 + p.dist_drive * 24.0;
            // Full scale in, full scale out, whatever the drive.
            let makeup = (1.0 + gain) / gain;
            let shape = |x: f32| soft_clip(x * gain) * makeup;
            left += (shape(left) - left) * p.dist_mix;
            right += (shape(right) - right) * p.dist_mix;
        }
        if p.chorus_on {
            (left, right) = self.chorus.process(p, self.sample_rate, left, right);
        }
        if p.delay_on {
            (left, right) = self.delay.process_stereo(left, right);
        }
        if p.reverb_on {
            (left, right) = self.reverb.process_stereo(left, right);
        }
        (left, right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn impulse_tail(p: &Params, frames: usize) -> f32 {
        let mut rack = FxRack::new(48_000.0);
        rack.configure(p);
        let mut energy = 0.0;
        for i in 0..frames {
            let x = if i == 0 { 1.0 } else { 0.0 };
            let (l, r) = rack.process(p, x, x);
            assert!(l.is_finite() && r.is_finite());
            if i > 100 {
                energy += l * l + r * r;
            }
        }
        energy
    }

    #[test]
    fn effects_off_pass_the_signal_through() {
        let p = crate::default_params();
        let mut rack = FxRack::new(48_000.0);
        assert_eq!(rack.process(&p, 0.25, -0.5), (0.25, -0.5));
        assert_eq!(impulse_tail(&p, 2_000), 0.0);
    }

    #[test]
    fn the_delay_and_reverb_leave_a_tail() {
        let p = Params {
            delay_on: true,
            delay_sync: false,
            delay_time_ms: 10.0,
            ..crate::default_params()
        };
        assert!(impulse_tail(&p, 4_000) > 1.0e-4);
        let p = Params {
            reverb_on: true,
            ..crate::default_params()
        };
        assert!(impulse_tail(&p, 24_000) > 1.0e-4);
    }

    #[test]
    fn distortion_keeps_full_scale() {
        let p = Params {
            dist_on: true,
            dist_drive: 1.0,
            dist_mix: 1.0,
            ..crate::default_params()
        };
        let mut rack = FxRack::new(48_000.0);
        let (l, _) = rack.process(&p, 1.0, 1.0);
        assert!((l - 1.0).abs() < 1.0e-5);
        let (quiet, _) = rack.process(&p, 0.05, 0.05);
        assert!(quiet > 0.05, "drive lifts a quiet signal");
    }
}

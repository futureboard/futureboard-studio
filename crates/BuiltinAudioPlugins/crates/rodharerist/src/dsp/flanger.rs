//! Jet-style stereo flanger — one short modulated delay line per channel with
//! regeneration, LFOs in quadrature for stereo width.

use builtin_dsp_core::mix;

use super::smooth::Smoothed;
use super::{InterpDelay, Lfo};

/// Longest modulated delay we ever read (ms) → buffer capacity.
const MAX_DELAY_MS: f32 = 12.0;

/// Glide time for depth/mix edits (see `smooth.rs`).
const SMOOTH_SECONDS: f32 = 0.010;

/// Fixed regeneration of the original jet. High enough for the metallic
/// comb, low enough that the loop can never run away (|fb| < 1).
const FEEDBACK: f32 = 0.45;

#[derive(Clone, Copy)]
pub(super) struct FlangerVoice {
    pub base_s: f32,
    pub depth_s: f32,
    pub rate_lo: f32,
    pub rate_span: f32,
    pub feedback: f32,
}

impl FlangerVoice {
    pub(super) fn classic() -> Self {
        Self {
            base_s: 0.0015,
            depth_s: 0.0012,
            rate_lo: 0.05,
            rate_span: 3.95,
            feedback: FEEDBACK,
        }
    }

    /// The sweep reaches nearly to the dry signal, and the feedback is negative.
    pub(super) fn thru_zero() -> Self {
        Self {
            base_s: 0.0022,
            depth_s: 0.0020,
            rate_lo: 0.08,
            rate_span: 2.4,
            feedback: -0.62,
        }
    }

    /// Slower, longer, more regeneration.
    pub(super) fn slow() -> Self {
        Self {
            base_s: 0.0045,
            depth_s: 0.0030,
            rate_lo: 0.02,
            rate_span: 0.7,
            feedback: 0.72,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Flanger {
    sample_rate: f32,
    line_l: InterpDelay,
    line_r: InterpDelay,
    lfo_l: Lfo,
    lfo_r: Lfo,
    base_samples: f32,
    depth_samples: Smoothed,
    mix: Smoothed,
    feedback: f32,
}

impl Flanger {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let capacity = ((sr * MAX_DELAY_MS * 0.001) as usize).max(4);
        let mut lfo_r = Lfo::new();
        lfo_r.set_phase(0.25); // 90° apart for stereo width
        Self {
            sample_rate: sr,
            line_l: InterpDelay::new(capacity),
            line_r: InterpDelay::new(capacity),
            lfo_l: Lfo::new(),
            lfo_r,
            base_samples: 0.0015 * sr,
            depth_samples: Smoothed::new(sr, SMOOTH_SECONDS, 0.0),
            mix: Smoothed::new(sr, SMOOTH_SECONDS, 0.5),
            feedback: FEEDBACK,
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        let capacity = ((sr * MAX_DELAY_MS * 0.001) as usize).max(4);
        self.sample_rate = sr;
        self.line_l = InterpDelay::new(capacity);
        self.line_r = InterpDelay::new(capacity);
        self.base_samples = 0.0015 * sr;
        self.depth_samples.set_time(sr, SMOOTH_SECONDS);
        self.mix.set_time(sr, SMOOTH_SECONDS);
    }

    pub(super) fn reset(&mut self) {
        self.line_l.clear();
        self.line_r.clear();
        self.lfo_l.reset();
        self.lfo_r.reset();
        self.lfo_r.set_phase(0.25);
        self.depth_samples.snap();
        self.mix.snap();
    }

    /// `rate` and `depth` are 0..10; `mix` is 0..100 %.
    pub(super) fn configure(&mut self, rate: f32, depth: f32, mix: f32) {
        self.configure_voice(rate, depth, mix, FlangerVoice::classic());
    }

    pub(super) fn configure_voice(&mut self, rate: f32, depth: f32, mix: f32, voice: FlangerVoice) {
        let sr = self.sample_rate;
        let rate_hz = voice.rate_lo + (rate / 10.0).clamp(0.0, 1.0) * voice.rate_span;
        self.lfo_l.set_rate(rate_hz, sr);
        self.lfo_r.set_rate(rate_hz, sr);
        self.base_samples = voice.base_s * sr;
        self.depth_samples
            .set_target((depth / 10.0).clamp(0.0, 1.0) * voice.depth_s * sr);
        self.feedback = voice.feedback;
        self.mix.set_target((mix / 100.0).clamp(0.0, 1.0));
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let depth = self.depth_samples.tick();
        let mix_amount = self.mix.tick();
        let mod_l = self.lfo_l.tick() * depth;
        let mod_r = self.lfo_r.tick() * depth;

        let wet_l = self.line_l.read_interp(self.base_samples + mod_l);
        let wet_r = self.line_r.read_interp(self.base_samples + mod_r);

        // Regeneration goes back into the line with the dry input.
        let feedback = self.feedback;
        self.line_l.write_sample(left + wet_l * feedback);
        self.line_r.write_sample(right + wet_r * feedback);

        (mix(left, wet_l, mix_amount), mix(right, wet_r, mix_amount))
    }
}

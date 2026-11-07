//! Amp-style stereo tremolo — LFO amplitude modulation with a shape control
//! that morphs the wave from smooth sine toward a choppy square.

use super::Lfo;
use super::smooth::Smoothed;

/// Glide time for depth/shape edits (see `smooth.rs`).
const SMOOTH_SECONDS: f32 = 0.010;

#[derive(Debug, Clone)]
pub(super) struct Tremolo {
    sample_rate: f32,
    lfo: Lfo,
    depth: Smoothed,
    /// 0 = pure sine, 1 = hard chop. Reuses the Mod slot's "mix" wire id.
    shape: Smoothed,
    kind: TremKind,
    /// Low band for the harmonic tremolo, one state per channel.
    split_l: f32,
    split_r: f32,
    split_a: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TremKind {
    Opto,
    Harmonic,
    Bias,
}

impl Tremolo {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        Self {
            sample_rate: sr,
            lfo: Lfo::new(),
            depth: Smoothed::new(sr, SMOOTH_SECONDS, 0.5),
            shape: Smoothed::new(sr, SMOOTH_SECONDS, 0.0),
            kind: TremKind::Opto,
            split_l: 0.0,
            split_r: 0.0,
            split_a: split_alpha(sr),
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        self.depth.set_time(sr, SMOOTH_SECONDS);
        self.shape.set_time(sr, SMOOTH_SECONDS);
        self.split_a = split_alpha(sr);
    }

    pub(super) fn reset(&mut self) {
        self.lfo.reset();
        self.depth.snap();
        self.shape.snap();
        self.split_l = 0.0;
        self.split_r = 0.0;
    }

    /// `rate` and `depth` are 0..10; `shape` is the Mod slot's 0..100 % knob.
    pub(super) fn configure(&mut self, rate: f32, depth: f32, shape: f32) {
        self.configure_kind(rate, depth, shape, TremKind::Opto);
    }

    pub(super) fn configure_kind(&mut self, rate: f32, depth: f32, shape: f32, kind: TremKind) {
        self.kind = kind;
        let (lo, span, depth_scale) = match kind {
            // 0.5 Hz → 12 Hz. The original opto.
            TremKind::Opto => (0.5, 11.5, 1.0),
            TremKind::Harmonic => (0.4, 8.0, 1.0),
            // Slower, and it never chops all the way down.
            TremKind::Bias => (0.15, 4.5, 0.55),
        };
        let rate_hz = lo + (rate / 10.0).clamp(0.0, 1.0) * span;
        self.lfo.set_rate(rate_hz, self.sample_rate);
        self.depth
            .set_target((depth / 10.0).clamp(0.0, 1.0) * depth_scale);
        self.shape.set_target((shape / 100.0).clamp(0.0, 1.0));
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let depth = self.depth.tick();
        let shape = self.shape.tick();
        let sine = self.lfo.tick();
        // Shape morph: drive the sine harder and soft-clip it toward a square.
        // `tanh(4x)` at full shape is a rounded chop with no aliasing spray.
        let drive = 1.0 + shape * 7.0;
        let shaped = (sine * drive).tanh() / (drive.min(4.0)).tanh().max(1.0e-3);
        // Unipolar gain, 1 at LFO peak, (1 - depth) at trough.
        let wave = shaped.clamp(-1.0, 1.0);
        match self.kind {
            TremKind::Opto | TremKind::Bias => {
                let gain = 1.0 - depth * 0.5 * (1.0 - wave);
                (left * gain, right * gain)
            }
            TremKind::Harmonic => {
                // Lows and highs pulse in opposite phase. One one-pole
                // per channel, no second oscillator.
                let a = self.split_a;
                self.split_l += a * (left - self.split_l);
                self.split_r += a * (right - self.split_r);
                let low_l = self.split_l;
                let low_r = self.split_r;
                let g_low = 1.0 - depth * 0.5 * (1.0 - wave);
                let g_high = 1.0 - depth * 0.5 * (1.0 + wave);
                (
                    low_l * g_low + (left - low_l) * g_high,
                    low_r * g_low + (right - low_r) * g_high,
                )
            }
        }
    }
}

fn split_alpha(sample_rate: f32) -> f32 {
    let sr = sample_rate.max(1.0);
    1.0 - (-std::f32::consts::TAU * 800.0 / sr).exp()
}

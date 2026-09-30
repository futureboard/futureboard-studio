//! The small DSP pieces the spatialisers are built from. All of them are
//! allocation-free after construction.

use std::f32::consts::PI;

/// One pole, one zero: `y = b0·x + b1·x[-1] − a1·y[-1]`. The head-shadow
/// filter, and (with `b1 = 0`) a gentle low-pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PoleZero {
    pub b0: f32,
    pub b1: f32,
    pub a1: f32,
}

impl PoleZero {
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        a1: 0.0,
    };

    /// One-pole low-pass at `cutoff_hz` (matched to the analogue pole).
    pub fn lowpass(cutoff_hz: f32, sample_rate: f32) -> Self {
        let cutoff = cutoff_hz.clamp(10.0, sample_rate * 0.49);
        let pole = (-2.0 * PI * cutoff / sample_rate).exp();
        Self {
            b0: 1.0 - pole,
            b1: 0.0,
            a1: -pole,
        }
    }

    /// Linear interpolation between two designs. Every design here keeps
    /// `|a1| < 1`, and so does any mix of two of them: sweeping between
    /// stable first-order filters stays stable.
    pub fn lerp(self, to: Self, t: f32) -> Self {
        Self {
            b0: self.b0 + (to.b0 - self.b0) * t,
            b1: self.b1 + (to.b1 - self.b1) * t,
            a1: self.a1 + (to.a1 - self.a1) * t,
        }
    }
}

/// State for [`PoleZero`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PoleZeroState {
    x1: f32,
    y1: f32,
}

impl PoleZeroState {
    #[inline]
    pub fn tick(&mut self, c: PoleZero, x: f32) -> f32 {
        let y = c.b0 * x + c.b1 * self.x1 - c.a1 * self.y1;
        self.x1 = x;
        // Flush denormals: a decaying tail must not slow the callback down.
        self.y1 = if y.abs() < 1.0e-20 { 0.0 } else { y };
        self.y1
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// A second-order low-pass (RBJ cookbook, Butterworth Q) — the LFE's 120 Hz
/// band limit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    pub fn lowpass(cutoff_hz: f32, sample_rate: f32) -> Self {
        let w0 = 2.0 * PI * cutoff_hz.clamp(10.0, sample_rate * 0.45) / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        Self {
            b0: (1.0 - cos) / 2.0 / a0,
            b1: (1.0 - cos) / a0,
            b2: (1.0 - cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = if y.abs() < 1.0e-20 { 0.0 } else { y };
        self.y1
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// A circular delay line read at fractional delays.
#[derive(Debug, Clone)]
pub(crate) struct DelayLine {
    buffer: Vec<f32>,
    mask: usize,
    write: usize,
}

impl DelayLine {
    /// Room for delays up to `max_delay` samples, plus interpolation margin.
    pub fn new(max_delay: usize) -> Self {
        let len = (max_delay + 8).next_power_of_two();
        Self {
            buffer: vec![0.0; len],
            mask: len - 1,
            write: 0,
        }
    }

    /// The longest delay a read may ask for.
    pub fn max_delay(&self) -> f32 {
        (self.buffer.len() - 4) as f32
    }

    #[inline]
    pub fn push(&mut self, x: f32) {
        self.write = (self.write + 1) & self.mask;
        self.buffer[self.write] = x;
    }

    #[inline]
    fn at(&self, back: usize) -> f32 {
        self.buffer[(self.write.wrapping_sub(back)) & self.mask]
    }

    /// The sample `delay` samples before the last one pushed, linearly
    /// interpolated. `0` is the last sample pushed.
    #[inline]
    pub fn read_linear(&self, delay: f32) -> f32 {
        let delay = delay.clamp(0.0, self.max_delay());
        let whole = delay as usize;
        let frac = delay - whole as f32;
        let a = self.at(whole);
        let b = self.at(whole + 1);
        a + (b - a) * frac
    }

    /// Four-point cubic (Catmull-Rom) read: flat to near Nyquist, for the
    /// interaural delay the ear localises most precisely by.
    #[inline]
    pub fn read_cubic(&self, delay: f32) -> f32 {
        let delay = delay.clamp(1.0, self.max_delay() - 2.0);
        let whole = delay as usize;
        let t = delay - whole as f32;
        let y0 = self.at(whole - 1);
        let y1 = self.at(whole);
        let y2 = self.at(whole + 1);
        let y3 = self.at(whole + 2);
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * t + c2) * t + c1) * t + y1
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }
}

/// Smoothstep on `0..=1`.
#[inline]
pub(crate) fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delay_line_returns_what_went_in_that_many_samples_ago() {
        let mut line = DelayLine::new(16);
        for i in 0..20 {
            line.push(i as f32);
        }
        assert_eq!(line.read_linear(0.0), 19.0);
        assert_eq!(line.read_linear(3.0), 16.0);
        assert!((line.read_linear(2.5) - 16.5).abs() < 1.0e-6);
        // A ramp is reproduced exactly by the cubic too.
        assert!((line.read_cubic(4.25) - 14.75).abs() < 1.0e-4);
    }

    #[test]
    fn the_lfe_filter_passes_bass_and_stops_treble() {
        let rate = 48_000.0;
        let gain_at = |hz: f32| {
            let mut filter = Biquad::lowpass(120.0, rate);
            let mut peak = 0.0f32;
            for n in 0..48_000 {
                let y = filter.tick((2.0 * PI * hz * n as f32 / rate).sin());
                if n > 24_000 {
                    peak = peak.max(y.abs());
                }
            }
            peak
        };
        assert!(gain_at(40.0) > 0.95);
        assert!(gain_at(2_000.0) < 0.01);
    }

    #[test]
    fn a_one_pole_lowpass_is_unity_at_dc() {
        let c = PoleZero::lowpass(1_000.0, 48_000.0);
        let mut s = PoleZeroState::default();
        let mut y = 0.0;
        for _ in 0..10_000 {
            y = s.tick(c, 1.0);
        }
        assert!((y - 1.0).abs() < 1.0e-4);
    }
}

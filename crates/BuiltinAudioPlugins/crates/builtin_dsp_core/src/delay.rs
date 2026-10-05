//! Delay-line building blocks shared by the time-based cores (VerbSpace,
//! EchoSpace): a power-of-two ring with fractional reads, a Schroeder
//! allpass, and a one-pole parameter smoother.
//!
//! All of them allocate once, at construction; the per-sample methods only
//! index and multiply.

use crate::flush_denormal;

/// A power-of-two ring read behind its write head.
#[derive(Debug, Clone)]
pub struct DelayRing {
    buffer: Box<[f32]>,
    mask: usize,
    /// Where the next sample goes.
    write: usize,
}

impl DelayRing {
    /// A ring holding at least `capacity` samples.
    pub fn new(capacity: usize) -> Self {
        let len = capacity.max(8).next_power_of_two();
        Self {
            buffer: vec![0.0; len].into_boxed_slice(),
            mask: len - 1,
            write: 0,
        }
    }

    /// A ring that can be read up to `ms` back at `sample_rate`, by any of
    /// the reads below.
    pub fn for_ms(ms: f32, sample_rate: f32) -> Self {
        Self::new((ms * 0.001 * sample_rate).ceil() as usize + 8)
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }

    /// Longest delay [`Self::read_cubic`] can take.
    pub fn max_cubic_delay(&self) -> f32 {
        (self.buffer.len() - 4) as f32
    }

    #[inline]
    pub fn push(&mut self, sample: f32) {
        self.buffer[self.write] = sample;
        self.write = (self.write + 1) & self.mask;
    }

    /// The sample `delay` whole samples back; 1 is the last one pushed.
    #[inline]
    pub fn at(&self, delay: usize) -> f32 {
        self.buffer[self.write.wrapping_sub(delay) & self.mask]
    }

    /// Linear read `delay` samples back, 1 being the last sample pushed.
    #[inline]
    pub fn read_linear(&self, delay: f32) -> f32 {
        let delay = crate::clamp(delay, 1.0, (self.buffer.len() - 2) as f32);
        let whole = delay as usize;
        let frac = delay - whole as f32;
        let newer = self.at(whole);
        newer + (self.at(whole + 1) - newer) * frac
    }

    /// Third-order Lagrange read `delay` samples back. Kept to the centre
    /// interval of its four points, where Lagrange interpolation never gains
    /// above unity — inside a feedback loop that is what keeps it stable —
    /// and, unlike a linear read, it does not dull the top end as the delay
    /// moves.
    #[inline]
    pub fn read_cubic(&self, delay: f32) -> f32 {
        let delay = crate::clamp(delay, 2.0, self.max_cubic_delay());
        let whole = delay as usize;
        let f = delay - whole as f32;
        let (p0, p1, p2, p3) = (
            self.at(whole - 1),
            self.at(whole),
            self.at(whole + 1),
            self.at(whole + 2),
        );
        let (fm1, fm2, fp1) = (f - 1.0, f - 2.0, f + 1.0);
        let c0 = -f * fm1 * fm2 * (1.0 / 6.0);
        let c1 = fp1 * fm1 * fm2 * 0.5;
        let c2 = -fp1 * f * fm2 * 0.5;
        let c3 = fp1 * f * fm1 * (1.0 / 6.0);
        p0 * c0 + p1 * c1 + p2 * c2 + p3 * c3
    }
}

/// Fixed-length Schroeder allpass. The buffer's length *is* the delay, so
/// there is no read offset to validate on the hot path. Unit magnitude at
/// every frequency: it smears, it never colours or gains.
#[derive(Debug, Clone)]
pub struct Allpass {
    buffer: Box<[f32]>,
    pos: usize,
}

impl Allpass {
    pub fn new(length: usize) -> Self {
        Self {
            buffer: vec![0.0; length.max(1)].into_boxed_slice(),
            pos: 0,
        }
    }

    /// An allpass `ms` long at `sample_rate`.
    pub fn for_ms(ms: f32, sample_rate: f32) -> Self {
        Self::new(((ms * 0.001 * sample_rate).round() as usize).max(1))
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.pos = 0;
    }

    #[inline]
    pub fn process(&mut self, input: f32, gain: f32) -> f32 {
        let delayed = self.buffer[self.pos];
        let stored = flush_denormal(input + delayed * gain);
        self.buffer[self.pos] = stored;
        self.pos += 1;
        if self.pos == self.buffer.len() {
            self.pos = 0;
        }
        delayed - stored * gain
    }
}

/// A value that moves toward its target by a fixed share each sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smoothed {
    pub value: f32,
    pub target: f32,
}

impl Smoothed {
    /// Settled at `value`.
    pub fn at(value: f32) -> Self {
        Self {
            value,
            target: value,
        }
    }

    /// One sample's move toward the target, by a share from
    /// [`smoothing_step`].
    #[inline]
    pub fn next(&mut self, step: f32) -> f32 {
        self.value += (self.target - self.value) * step;
        self.value
    }

    /// Lands on the target.
    pub fn settle(&mut self) {
        self.value = self.target;
    }
}

/// Per-sample share for a one-pole smoother with time constant `ms`.
pub fn smoothing_step(ms: f32, sample_rate: f32) -> f32 {
    1.0 - (-1.0 / (ms * 0.001 * sample_rate).max(1.0)).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cubic_read_is_exact_on_a_cubic() {
        let mut ring = DelayRing::new(64);
        let curve = |n: f32| 0.001 * n * n * n - 0.02 * n * n + 0.3 * n;
        for n in 0..40 {
            ring.push(curve(n as f32));
        }
        // The last sample pushed (n = 39) sits 1 behind the head.
        for delay in [2.0f32, 2.25, 5.5, 13.75] {
            let expected = curve(40.0 - delay);
            assert!((ring.read_cubic(delay) - expected).abs() < 1.0e-3);
        }
    }

    #[test]
    fn a_linear_read_lands_between_the_right_samples() {
        let mut ring = DelayRing::new(16);
        for sample in 0..12 {
            ring.push(sample as f32);
        }
        // 1 back is 11, 2 back is 10.
        assert!((ring.read_linear(1.25) - 10.75).abs() < 1.0e-6);
    }

    /// Inside a loop an interpolator that gains even a little above unity at
    /// some fraction builds without bound.
    #[test]
    fn a_cubic_read_never_gains_above_unity() {
        for step in 0..20 {
            let frac = step as f32 / 20.0;
            let c = |f: f32| {
                let (fm1, fm2, fp1) = (f - 1.0, f - 2.0, f + 1.0);
                [
                    -f * fm1 * fm2 / 6.0,
                    fp1 * fm1 * fm2 * 0.5,
                    -fp1 * f * fm2 * 0.5,
                    fp1 * f * fm1 / 6.0,
                ]
            };
            let taps = c(frac);
            for bin in 0..64 {
                let w = std::f32::consts::PI * bin as f32 / 64.0;
                let (re, im) = taps
                    .iter()
                    .enumerate()
                    .fold((0.0, 0.0), |(re, im), (k, t)| {
                        (re + t * (w * k as f32).cos(), im - t * (w * k as f32).sin())
                    });
                assert!(
                    (re * re + im * im).sqrt() <= 1.0 + 1.0e-4,
                    "gain above unity at frac {frac}, w {w}"
                );
            }
        }
    }

    #[test]
    fn an_allpass_keeps_the_energy() {
        let mut ap = Allpass::new(37);
        let mut energy_in = 0.0f32;
        let mut energy_out = 0.0f32;
        for n in 0..20_000 {
            let x = if n < 400 {
                ((n * 7919) % 101) as f32 / 50.0 - 1.0
            } else {
                0.0
            };
            let y = ap.process(x, 0.7);
            energy_in += x * x;
            energy_out += y * y;
        }
        assert!((energy_in - energy_out).abs() < energy_in * 1.0e-3);
    }

    #[test]
    fn a_smoother_settles_at_its_target() {
        let step = smoothing_step(10.0, 48_000.0);
        let mut value = Smoothed::at(0.0);
        value.target = 1.0;
        for _ in 0..4_800 {
            value.next(step);
        }
        assert!((value.value - 1.0).abs() < 1.0e-3);
    }
}

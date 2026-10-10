//! The clipper's building blocks: a mirrored ring (delay line and FIR
//! window in one), the polyphase anti-alias filter its oversampling runs
//! through, and the lookahead peak limiter Hybrid and Limit use.
//!
//! Everything here is sized at construction; nothing allocates afterwards.

/// Taps per polyphase branch of the anti-alias filter. Upsampling and
/// downsampling each delay by half the filter, so a round trip delays by
/// exactly this many samples at the host rate, whatever the factor.
pub const TAPS_PER_PHASE: usize = 32;

/// The highest oversampling factor.
pub const MAX_FACTOR: usize = 8;

/// Kaiser β for roughly 80 dB of stopband rejection.
const KAISER_BETA: f64 = 7.857;

/// The filter's −6 dB point as a fraction of the host rate: just under
/// Nyquist, so what folds back lands above ~0.45·fs and the stopband starts
/// near 0.55·fs.
const CUTOFF: f64 = 0.47;

/// A delay line read newest-first, mirrored so any window up to `cap` is one
/// contiguous slice: `recent(n)[i]` is the sample `i` pushes ago.
#[derive(Debug, Clone)]
pub struct Ring {
    buf: Box<[f32]>,
    cap: usize,
    pos: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            buf: vec![0.0; cap * 2].into_boxed_slice(),
            cap,
            pos: 0,
        }
    }

    #[inline]
    pub fn push(&mut self, x: f32) {
        self.pos = if self.pos == 0 {
            self.cap - 1
        } else {
            self.pos - 1
        };
        self.buf[self.pos] = x;
        self.buf[self.pos + self.cap] = x;
    }

    /// The sample `delay` pushes ago (`0` is the newest).
    #[inline]
    pub fn at(&self, delay: usize) -> f32 {
        self.buf[self.pos + delay.min(self.cap - 1)]
    }

    /// The newest `len` samples, newest first.
    #[inline]
    pub fn recent(&self, len: usize) -> &[f32] {
        &self.buf[self.pos..self.pos + len.min(self.cap)]
    }

    /// `len` samples starting `offset` pushes ago, newest first: the window
    /// [`Self::recent`] gave `offset` pushes back. `offset + len` stays
    /// within the capacity.
    #[inline]
    pub fn recent_from(&self, offset: usize, len: usize) -> &[f32] {
        let offset = offset.min(self.cap);
        let start = self.pos + offset;
        &self.buf[start..start + len.min(self.cap - offset)]
    }

    pub fn clear(&mut self) {
        self.fill(0.0);
    }

    pub fn fill(&mut self, value: f32) {
        self.buf.fill(value);
    }
}

/// Zeroth-order modified Bessel function of the first kind (Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let half = x * 0.5;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..64 {
        term *= half / k as f64;
        let add = term * term;
        sum += add;
        if add < sum * 1.0e-16 {
            break;
        }
    }
    sum
}

/// The anti-alias lowpass for one factor `M`: `M·K + 1` linear-phase taps,
/// cut at [`CUTOFF`] of the host rate, unity DC gain.
#[derive(Debug, Clone)]
pub struct AntiAlias {
    /// The whole filter, for the decimator.
    pub taps: Box<[f32]>,
    /// `M·taps[k·M + p]` for each phase `p`, `K + 1` taps each (zero past
    /// the end), for the interpolator.
    pub phases: Box<[f32]>,
    /// The most the interpolator can raise a peak: the largest phase's sum
    /// of absolute taps. A host-rate window below `level / bound` cannot put
    /// any upsampled sample above `level`.
    pub peak_bound: f32,
}

impl AntiAlias {
    pub fn new(factor: usize) -> Self {
        let m = factor.max(1);
        let len = m * TAPS_PER_PHASE + 1;
        let center = (len - 1) as f64 * 0.5;
        let wc = std::f64::consts::PI * 2.0 * CUTOFF / m as f64;
        let norm = bessel_i0(KAISER_BETA);
        let mut taps: Vec<f64> = (0..len)
            .map(|n| {
                let t = n as f64 - center;
                let sinc = if t.abs() < 1.0e-12 {
                    wc / std::f64::consts::PI
                } else {
                    (wc * t).sin() / (std::f64::consts::PI * t)
                };
                let r = (2.0 * n as f64 / (len - 1) as f64) - 1.0;
                let window = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / norm;
                sinc * window
            })
            .collect();
        let sum: f64 = taps.iter().sum();
        for tap in &mut taps {
            *tap /= sum;
        }

        let per = TAPS_PER_PHASE + 1;
        let mut phases = vec![0.0f32; m * per];
        let mut peak_bound = 0.0f32;
        for p in 0..m {
            let mut abs_sum = 0.0f64;
            for k in 0..per {
                let n = k * m + p;
                if n < len {
                    let v = taps[n] * m as f64;
                    phases[p * per + k] = v as f32;
                    abs_sum += v.abs();
                }
            }
            peak_bound = peak_bound.max(abs_sum as f32);
        }
        Self {
            taps: taps.iter().map(|&t| t as f32).collect(),
            phases: phases.into_boxed_slice(),
            peak_bound,
        }
    }

    /// Upsampled sample `phase` of the newest host-rate input in `history`
    /// (newest first, at least `K + 1` long).
    #[inline]
    pub fn interpolate(&self, history: &[f32], phase: usize) -> f32 {
        let per = TAPS_PER_PHASE + 1;
        let taps = &self.phases[phase * per..phase * per + per];
        taps.iter().zip(history).map(|(h, x)| h * x).sum()
    }

    /// One decimated output from `window` (newest first, `taps.len()` long).
    #[inline]
    pub fn decimate(&self, window: &[f32]) -> f32 {
        self.taps.iter().zip(window).map(|(h, x)| h * x).sum()
    }
}

/// A lookahead peak limiter's gain computer: a sliding minimum of the gain
/// each sample needs, released exponentially, then averaged over the same
/// window. The average reaches a peak's gain exactly when that peak leaves a
/// delay of `window − 1` samples, so the delayed audio never passes the
/// target (Signalsmith's hold-and-box design).
#[derive(Debug, Clone)]
pub struct Lookahead {
    window: usize,
    // Monotonic deque of (time, gain), as a ring of `mask + 1` slots.
    dq_gain: Box<[f32]>,
    dq_time: Box<[u64]>,
    head: usize,
    len: usize,
    mask: usize,
    time: u64,
    release: f32,
    held: f32,
    boxcar: Ring,
    sum: f64,
    reduced: usize,
}

impl Lookahead {
    /// Room for a window of up to `max_window` samples.
    pub fn new(max_window: usize) -> Self {
        let slots = (max_window + 1).next_power_of_two();
        let mut limiter = Self {
            window: 1,
            dq_gain: vec![1.0; slots].into_boxed_slice(),
            dq_time: vec![0; slots].into_boxed_slice(),
            head: 0,
            len: 0,
            mask: slots - 1,
            time: 0,
            release: 0.0,
            held: 1.0,
            boxcar: Ring::new(max_window + 1),
            sum: 1.0,
            reduced: 0,
        };
        limiter.configure(1, 0.0);
        limiter
    }

    /// A window of `window` samples (the audio is delayed `window − 1`) and
    /// a release coefficient; clears the state.
    pub fn configure(&mut self, window: usize, release: f32) {
        self.window = window.clamp(1, self.mask);
        self.release = release;
        self.reset();
    }

    pub fn reset(&mut self) {
        self.head = 0;
        self.len = 0;
        self.time = 0;
        self.held = 1.0;
        // The boxcar starts full of unity.
        self.boxcar.fill(1.0);
        self.sum = self.window as f64;
        self.reduced = 0;
    }

    /// The gain for the sample `window − 1` pushes ago, given the gain the
    /// newest sample needs (`1` when it is under the target).
    #[inline]
    pub fn next(&mut self, needed: f32) -> f32 {
        let t = self.time;
        self.time += 1;
        // Sliding minimum over the window.
        while self.len > 0 {
            let back = (self.head + self.len - 1) & self.mask;
            if self.dq_gain[back] >= needed {
                self.len -= 1;
            } else {
                break;
            }
        }
        let slot = (self.head + self.len) & self.mask;
        self.dq_gain[slot] = needed;
        self.dq_time[slot] = t;
        self.len += 1;
        let window = self.window as u64;
        while self.dq_time[self.head] + window <= t {
            self.head = (self.head + 1) & self.mask;
            self.len -= 1;
        }
        let minimum = self.dq_gain[self.head];

        // Instant attack (the boxcar ramps it), exponential release.
        if minimum <= self.held {
            self.held = minimum;
        } else {
            self.held = minimum + self.release * (self.held - minimum);
            if minimum - self.held < 1.0e-6 {
                self.held = minimum;
            }
        }

        // Boxcar average over the window.
        let leaving = self.boxcar.at(self.window - 1);
        self.boxcar.push(self.held);
        self.sum += f64::from(self.held) - f64::from(leaving);
        self.reduced += usize::from(self.held < 1.0);
        self.reduced -= usize::from(leaving < 1.0);
        if self.reduced == 0 {
            self.sum = self.window as f64;
            1.0
        } else {
            (self.sum / self.window as f64) as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_passes_dc_and_cuts_past_nyquist() {
        for factor in [2, 4, 8] {
            let f = AntiAlias::new(factor);
            assert_eq!(f.taps.len(), factor * TAPS_PER_PHASE + 1);
            let response = |freq: f64| {
                // freq as a fraction of the host rate.
                let w = std::f64::consts::TAU * freq / factor as f64;
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (n, &h) in f.taps.iter().enumerate() {
                    re += f64::from(h) * (w * n as f64).cos();
                    im -= f64::from(h) * (w * n as f64).sin();
                }
                20.0 * (re * re + im * im).sqrt().max(1.0e-12).log10()
            };
            assert!(response(0.0).abs() < 1.0e-3);
            assert!(response(0.35).abs() < 0.05, "passband at {factor}x");
            assert!(response(0.6) < -70.0, "stopband at {factor}x");
            assert!(
                // A sinc interpolator's worst case grows with its length.
                f.peak_bound >= 1.0 && f.peak_bound < 3.0,
                "{factor}x bound {}",
                f.peak_bound
            );
        }
    }

    #[test]
    fn the_limiter_holds_a_peak_to_its_target_after_the_delay() {
        let window = 48;
        let mut limiter = Lookahead::new(window);
        limiter.configure(window, 0.999);
        let mut delayed = Ring::new(window + 1);
        let mut worst = 0.0f32;
        for n in 0..2_000 {
            let x = if n % 300 == 100 { 4.0 } else { 0.5 };
            delayed.push(x);
            let needed = if x > 1.0 { 1.0 / x } else { 1.0 };
            let gain = limiter.next(needed);
            worst = worst.max(gain * delayed.at(window - 1));
        }
        assert!(worst <= 1.0 + 1.0e-5, "{worst}");
    }
}

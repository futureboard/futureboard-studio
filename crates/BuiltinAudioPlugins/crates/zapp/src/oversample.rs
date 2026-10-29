//! The oversampler's building blocks: a mirrored ring (delay line and FIR
//! window in one) and the polyphase Kaiser anti-alias filter.
//!
//! Mirrors `67Clipper`'s `oversample.rs` (same filter design, same latency
//! contract) without depending on that plug-in's crate. Everything here is
//! sized at construction; nothing allocates afterwards.

/// Taps per polyphase branch of the anti-alias filter. Upsampling and
/// downsampling each delay by half the filter, so a round trip delays by
/// exactly this many samples at the host rate, whatever the factor.
pub const TAPS_PER_PHASE: usize = 32;

/// The highest oversampling factor.
pub const MAX_FACTOR: usize = 8;

/// Kaiser β for roughly 80 dB of stopband rejection.
const KAISER_BETA: f64 = 7.857;

/// The filter's −6 dB point as a fraction of the host rate: just under
/// Nyquist, so what folds back lands above ~0.45·fs.
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

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
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
        for p in 0..m {
            for k in 0..per {
                let n = k * m + p;
                if n < len {
                    phases[p * per + k] = (taps[n] * m as f64) as f32;
                }
            }
        }
        Self {
            taps: taps.iter().map(|&t| t as f32).collect(),
            phases: phases.into_boxed_slice(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_passes_dc_and_has_unity_phases() {
        for factor in [2, 4, 8] {
            let filter = AntiAlias::new(factor);
            let sum: f32 = filter.taps.iter().sum();
            assert!((sum - 1.0).abs() < 1.0e-4);
            let ones = [1.0f32; TAPS_PER_PHASE + 1];
            for phase in 0..factor {
                assert!((filter.interpolate(&ones, phase) - 1.0).abs() < 1.0e-2);
            }
        }
    }
}

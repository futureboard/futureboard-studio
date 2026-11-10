//! Dual-head delay-line pitch shifter.
//!
//! One circular buffer per channel, allocated in [`PitchShift::new`] and
//! [`PitchShift::set_sample_rate`]. `process` only writes, reads, and
//! advances two crossfaded heads. A ratio of 1 (no shift, and not the
//! detune pair) fades back to the dry sample, so a transpose of zero does
//! not delay or colour the rig.

use super::flush_denormal;

/// Grain length. Half of this is the delay a shifted signal carries.
const WINDOW_SECONDS: f32 = 0.04;
/// How fast the dry/shifted blend moves, so engaging a shift does not click.
const BLEND_SECONDS: f32 = 0.012;

/// Lowest and highest ratio `process` will track (±2 octaves).
const MIN_RATIO: f32 = 0.25;
const MAX_RATIO: f32 = 4.0;

pub(super) struct PitchShift {
    sample_rate: f32,
    left: Vec<f32>,
    right: Vec<f32>,
    mask: usize,
    write: usize,
    window: f32,
    phase_a: f32,
    phase_b: f32,
    blend: f32,
    blend_step: f32,
}

impl PitchShift {
    pub(super) fn new(sample_rate: f32) -> Self {
        let mut shift = Self {
            sample_rate: 1.0,
            left: Vec::new(),
            right: Vec::new(),
            mask: 0,
            write: 0,
            window: 1.0,
            phase_a: 0.25,
            phase_b: 0.25,
            blend: 0.0,
            blend_step: 1.0,
        };
        shift.set_sample_rate(sample_rate);
        shift
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        // Four grains of storage, a power of two, so a head never wraps
        // into the write it just passed.
        let window = (sr * WINDOW_SECONDS).round().max(64.0);
        let len = (window as usize * 8).next_power_of_two().max(512);
        self.left = vec![0.0; len];
        self.right = vec![0.0; len];
        self.mask = len - 1;
        self.window = window.min((len / 4) as f32);
        self.blend_step = 1.0 / (sr * BLEND_SECONDS).max(1.0);
        self.reset();
    }

    pub(super) fn reset(&mut self) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        self.write = 0;
        self.phase_a = 0.25;
        self.phase_b = 0.25;
        self.blend = 0.0;
    }

    /// Samples of delay a shifted signal carries once the blend has opened.
    /// The host should use [`Self::delay_samples`] together with whether the
    /// shift is engaged: the blend is still closed on the control-thread
    /// edit that turns a transpose on.
    pub(super) fn latency_samples(&self) -> usize {
        if self.blend < 1.0e-3 {
            0
        } else {
            (self.window * 0.5).round() as usize
        }
    }

    /// Delay a shifted signal carries, whether or not the blend has opened.
    pub(super) fn delay_samples(&self) -> usize {
        (self.window * 0.5).round() as usize
    }

    /// `ratio` is the pitch multiplier (2.0 is an octave up). `dual` runs a
    /// second voice at the reciprocal ratio and averages the two: the detune
    /// mode. Unity with `dual` clear returns the input.
    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32, ratio: f32, dual: bool) -> (f32, f32) {
        let ratio = if ratio.is_finite() {
            ratio.clamp(MIN_RATIO, MAX_RATIO)
        } else {
            1.0
        };
        let unity = !dual && (ratio - 1.0).abs() < 1.0e-4;
        let target = if unity { 0.0 } else { 1.0 };
        let write = self.write;
        let mask = self.mask;
        let window = self.window;
        let phase = self.phase_a;
        let wet_l = voice(&mut self.left, mask, write, phase, left, ratio, window);
        let wet_r = voice(&mut self.right, mask, write, phase, right, ratio, window);
        self.phase_a = wrap_phase(phase + phase_step(ratio, window));
        let (wet_l, wet_r) = if dual {
            let down = 1.0 / ratio;
            let phase_b = self.phase_b;
            let b_l = voice(&mut self.left, mask, write, phase_b, left, down, window);
            let b_r = voice(&mut self.right, mask, write, phase_b, right, down, window);
            self.phase_b = wrap_phase(phase_b + phase_step(down, window));
            (0.5 * (wet_l + b_l), 0.5 * (wet_r + b_r))
        } else {
            (wet_l, wet_r)
        };
        self.write = (write + 1) & mask;
        self.blend += (target - self.blend).clamp(-self.blend_step, self.blend_step);
        if self.blend <= 1.0e-4 {
            self.blend = 0.0;
            return (left, right);
        }
        let k = self.blend;
        (
            flush_denormal(left + (wet_l - left) * k),
            flush_denormal(right + (wet_r - right) * k),
        )
    }
}

/// Writes `x` and returns the crossfaded read at `ratio`. Does not advance `phase`.
#[inline]
fn voice(
    buf: &mut [f32],
    mask: usize,
    write: usize,
    phase: f32,
    x: f32,
    _ratio: f32,
    window: f32,
) -> f32 {
    buf[write] = x;
    let a = read_at(buf, mask, write, phase * window);
    let b = read_at(buf, mask, write, wrap_phase(phase + 0.5) * window);
    // Equal-power: sin^2 + cos^2 over the grain, so the handoff is flat.
    let w_a = (std::f32::consts::PI * phase).sin();
    let w_b = (std::f32::consts::PI * wrap_phase(phase + 0.5)).sin();
    flush_denormal(a * w_a + b * w_b)
}

#[inline]
fn phase_step(ratio: f32, window: f32) -> f32 {
    (1.0 - ratio) / window
}

#[inline]
fn wrap_phase(phase: f32) -> f32 {
    let wrapped = phase.rem_euclid(1.0);
    if wrapped.is_finite() {
        wrapped
    } else {
        0.0
    }
}

#[inline]
fn read_at(buf: &[f32], mask: usize, write: usize, delay: f32) -> f32 {
    let delay = delay.max(1.0);
    let pos = write as f32 - delay;
    let base = pos.floor();
    let frac = pos - base;
    let i0 = (base as isize).rem_euclid((mask as isize) + 1) as usize;
    let i1 = (i0 + 1) & mask;
    buf[i0] + (buf[i1] - buf[i0]) * frac
}

/// Pitch ratio for `semitones` (12 is an octave up).
#[inline]
pub(super) fn semitone_ratio(semitones: f32) -> f32 {
    if !semitones.is_finite() || semitones.abs() < 1.0e-4 {
        return 1.0;
    }
    (semitones * (std::f32::consts::LN_2 / 12.0)).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dominant_hz(samples: &[f32], sample_rate: f32) -> f32 {
        // Zero-crossing rate over the settled tail. Enough to tell an
        // octave from the input; not a pitch tracker.
        let start = samples.len() / 2;
        let mut crossings = 0.0f32;
        for pair in samples[start..].windows(2) {
            if pair[0] <= 0.0 && pair[1] > 0.0 {
                crossings += 1.0;
            }
        }
        let seconds = (samples.len() - start) as f32 / sample_rate;
        crossings / seconds.max(1.0e-6)
    }

    #[test]
    fn unity_ratio_is_the_dry_sample() {
        let mut shift = PitchShift::new(48_000.0);
        for n in 0..4_000 {
            let x = (n as f32 * 0.05).sin() * 0.4;
            let (l, r) = shift.process(x, x, 1.0, false);
            assert!(
                (l - x).abs() < 1.0e-5 && (r - x).abs() < 1.0e-5,
                "{l} {r} vs {x}"
            );
        }
        assert_eq!(shift.latency_samples(), 0);
    }

    #[test]
    fn an_octave_up_doubles_the_rate() {
        let sr = 48_000.0;
        let mut shift = PitchShift::new(sr);
        let mut out = Vec::with_capacity(16_000);
        for n in 0..16_000 {
            let x = (n as f32 * std::f32::consts::TAU * 220.0 / sr).sin() * 0.5;
            let (l, _) = shift.process(x, x, 2.0, false);
            out.push(l);
        }
        let hz = dominant_hz(&out, sr);
        assert!(
            (hz - 440.0).abs() < 30.0,
            "octave up read {hz} Hz, expected near 440"
        );
    }
}

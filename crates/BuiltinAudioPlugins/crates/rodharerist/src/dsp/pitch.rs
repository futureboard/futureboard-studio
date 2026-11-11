//! Correlation-spliced delay-line pitch shifter.
//!
//! Each voice reads the input through a delay line whose delay moves by
//! `1 - ratio` per sample, so the read head plays the input `ratio` times as
//! fast. When the delay runs out of room the head has to jump: a pitch up
//! jumps back (repeats a stretch), a pitch down jumps forward (skips one).
//! Where it lands decides how the splice sounds. A blind jump lands at an
//! arbitrary phase of the waveform and the crossfade cancels part of the
//! signal: the periodic dip and warble that reads as "robotic".
//!
//! Here the jump is chosen: among landing points one guitar period wide, the
//! one whose surroundings best match the outgoing head's (normalised
//! cross-correlation, coarse grid then a fine pass around the winner) wins,
//! and the crossfade shape follows how well they matched — amplitude
//! complementary for an in-phase match, closer to equal power when the
//! material does not line up (a chord, noise).
//!
//! Realtime: the circular buffers (left, right, and a low-passed mono copy the
//! search reads) are allocated in [`PitchShift::new`] and
//! [`PitchShift::set_sample_rate`]. `process` only writes, reads, and runs a
//! bounded search at a splice: about 10k multiply-adds per voice, the same
//! count at every sample rate, a few times per 10 ms.
//!
//! A ratio of 1 (no shift, and not the detune pair) fades back to the dry
//! sample, so a transpose of zero does not delay or colour the rig.

use super::flush_denormal;

/// Smallest delay a head reaches before it must jump back (pitch up).
const MIN_DELAY_SECONDS: f32 = 0.0015;
/// Shortest jump. With the fade, sets how often a large shift splices.
const MIN_JUMP_SECONDS: f32 = 0.004;
/// Width of the landing range above the shortest jump: one period of a
/// seven-string's low B (62 Hz), so an in-phase landing point exists for
/// any guitar note, and two periods of most chords' lowest tones.
const SEARCH_SPAN_SECONDS: f32 = 0.016;
/// Crossfade length bounds. Small shifts get the long fade; a large shift
/// splices more often and gets a shorter one.
const MIN_FADE_SECONDS: f32 = 0.001;
const MAX_FADE_SECONDS: f32 = 0.006;
/// Stretch of signal the search compares around each head.
const MATCH_SECONDS: f32 = 0.006;
/// Delay reported for a shifted signal. The heads wander between
/// [`MIN_DELAY_SECONDS`] and about 25 ms; their average sits within a few
/// ms of this — a little under for a shift up, a little over for one down.
const LATENCY_SECONDS: f32 = 0.012;
/// How fast the dry/shifted blend moves, so engaging a shift does not click.
const BLEND_SECONDS: f32 = 0.012;
/// Time constant a ratio change glides with.
const GLIDE_SECONDS: f32 = 0.015;
/// Grid of the coarse search (candidates and window points).
const COARSE_RATE_HZ: f32 = 6_000.0;
/// Window point spacing of the fine pass.
const FINE_STRIDE_RATE_HZ: f32 = 24_000.0;
/// Candidate spacing of the fine pass.
const REFINE_RATE_HZ: f32 = 48_000.0;
/// The search reads a low-passed copy so the coarse grid does not alias.
const ANALYSIS_LOWPASS_HZ: f32 = 2_500.0;
/// A fade this many times shorter than the jump interval leaves the head
/// at rest between splices.
const FADE_SHARE: f32 = 0.75;
/// Score a landing point gains for being at the far end of the range: among
/// near-equal matches the longer jump wins, so a steady note splices less.
const LONG_JUMP_BIAS: f32 = 0.08;

/// Lowest and highest ratio `process` will track (±2 octaves).
const MIN_RATIO: f32 = 0.25;
const MAX_RATIO: f32 = 4.0;

/// Sample counts derived from the rate. Fixed between `set_sample_rate` calls.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    min_delay: f32,
    min_jump: f32,
    span: f32,
    min_fade: f32,
    max_fade: f32,
    /// Longest delay a head is allowed: the ring minus the search window.
    max_delay: f32,
    /// Ring length, a power of two.
    ring: usize,
    match_len: usize,
    coarse_step: usize,
    fine_stride: usize,
    refine_step: usize,
}

impl Geometry {
    fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1_000.0);
        let samples = |seconds: f32| (sr * seconds).max(1.0);
        let step = |rate: f32| ((sr / rate).round() as usize).max(1);
        let match_len = (samples(MATCH_SECONDS).round() as usize).max(16);
        let mut geo = Self {
            min_delay: samples(MIN_DELAY_SECONDS).max(3.0),
            min_jump: samples(MIN_JUMP_SECONDS),
            span: samples(SEARCH_SPAN_SECONDS),
            min_fade: samples(MIN_FADE_SECONDS),
            max_fade: samples(MAX_FADE_SECONDS),
            max_delay: 0.0,
            ring: 0,
            match_len,
            coarse_step: step(COARSE_RATE_HZ),
            fine_stride: step(FINE_STRIDE_RATE_HZ),
            refine_step: step(REFINE_RATE_HZ),
        };
        let reach = geo.reach();
        // Room past the deepest head for the search window behind it, and
        // for a fade a ratio change stretches.
        geo.ring = ((reach + (2 * match_len) as f32).ceil() as usize)
            .next_power_of_two()
            .max(512);
        geo.max_delay = (geo.ring - match_len - 8) as f32;
        geo
    }

    /// Deepest delay a head reaches at any ratio: a two-octave shift up
    /// (deepest landing) or down (latest splice plus the fade after it).
    fn reach(&self) -> f32 {
        let (fade_up, jump_up) = self.plan(MAX_RATIO);
        let up = self.min_delay + (MAX_RATIO - 1.0) * fade_up + jump_up + self.span;
        let (fade_down, jump_down) = self.plan(MIN_RATIO);
        let down = self.min_delay + jump_down + self.span + (1.0 - MIN_RATIO) * fade_down;
        up.max(down)
    }

    /// `(fade, shortest jump)` for `ratio`, in samples. The jump interval
    /// (`jump / |ratio - 1|`) always outlasts the fade.
    #[inline]
    fn plan(&self, ratio: f32) -> (f32, f32) {
        let dev = (ratio - 1.0).abs().max(1.0e-6);
        let fade = (FADE_SHARE * self.min_jump / dev).clamp(self.min_fade, self.max_fade);
        let jump = self.min_jump.max(dev * fade / FADE_SHARE);
        (fade, jump)
    }

    /// Delay a head starts from when a shift engages: low, so the blend
    /// from the dry signal is short, and with room for the first fade.
    #[inline]
    fn start_delay(&self, ratio: f32) -> f32 {
        if ratio > 1.0 {
            let (fade, _) = self.plan(ratio);
            self.min_delay + (ratio - 1.0) * fade
        } else {
            self.min_delay
        }
    }
}

/// The rings a voice reads, borrowed for one frame. `write` holds the
/// current input.
#[derive(Clone, Copy)]
struct Ring<'a> {
    left: &'a [f32],
    right: &'a [f32],
    /// Low-passed mono: what the coarse search compares.
    ana: &'a [f32],
    mask: usize,
    write: usize,
}

impl Ring<'_> {
    #[inline]
    fn read(&self, delay: f32) -> (f32, f32) {
        (
            read_at(self.left, self.mask, self.write, delay),
            read_at(self.right, self.mask, self.write, delay),
        )
    }

    /// Full-band mono at ring index `i`.
    #[inline]
    fn mono(&self, i: usize) -> f32 {
        let i = i & self.mask;
        self.left[i] + self.right[i]
    }
}

/// One read voice: two heads, one playing, the other faded in at a splice.
#[derive(Debug, Clone, Copy)]
struct Voice {
    delay: [f32; 2],
    active: usize,
    /// Samples into the current fade; `fade_len == 0` when not fading.
    fade_pos: f32,
    fade_len: f32,
    /// Correlation of the splice in progress, 0..1: shapes the fade.
    rho: f32,
}

impl Voice {
    fn parked(delay: f32) -> Self {
        Self {
            delay: [delay; 2],
            active: 0,
            fade_pos: 0.0,
            fade_len: 0.0,
            rho: 0.0,
        }
    }

    /// One output frame at `ratio`.
    #[inline]
    fn tick(&mut self, ring: &Ring, ratio: f32, geo: &Geometry) -> (f32, f32) {
        if self.fade_len <= 0.0 {
            let d = self.delay[self.active];
            let (fade, jump_lo) = geo.plan(ratio);
            let jump_hi = jump_lo + geo.span;
            if ratio > 1.0 && d <= geo.min_delay + (ratio - 1.0) * fade {
                self.splice(ring, true, fade, jump_lo, jump_hi, geo);
            } else if ratio < 1.0 && d >= geo.min_delay + jump_hi {
                self.splice(ring, false, fade, jump_lo, jump_hi, geo);
            }
        }
        let a = self.active;
        let (mut l, mut r) = ring.read(self.delay[a]);
        if self.fade_len > 0.0 {
            let b = a ^ 1;
            let t = ((self.fade_pos + 0.5) / self.fade_len).min(1.0);
            let w_in = t * t * (3.0 - 2.0 * t);
            let w_out = 1.0 - w_in;
            // Unit power for two signals correlated by `rho`: a plain
            // complementary fade for an in-phase match, equal power for
            // material that does not line up.
            let power = w_out * w_out + w_in * w_in + 2.0 * self.rho * w_out * w_in;
            let norm = 1.0 / power.max(0.25).sqrt();
            let (b_l, b_r) = ring.read(self.delay[b]);
            l = (l * w_out + b_l * w_in) * norm;
            r = (r * w_out + b_r * w_in) * norm;
            self.fade_pos += 1.0;
            if self.fade_pos >= self.fade_len {
                self.active = b;
                self.fade_len = 0.0;
                self.fade_pos = 0.0;
            }
        }
        let step = 1.0 - ratio;
        for d in &mut self.delay {
            *d = (*d + step).clamp(1.0, geo.max_delay);
        }
        (flush_denormal(l), flush_denormal(r))
    }

    /// Starts a fade to the best-matching landing point `jump_lo..=jump_hi`
    /// samples older (`up`) or newer than the playing head.
    #[inline]
    fn splice(
        &mut self,
        ring: &Ring,
        up: bool,
        fade: f32,
        jump_lo: f32,
        jump_hi: f32,
        geo: &Geometry,
    ) {
        let from = self.delay[self.active];
        let (jump, rho) = best_jump(ring, from, up, jump_lo, jump_hi, geo);
        let next = if up { from + jump } else { from - jump };
        self.delay[self.active ^ 1] = next.clamp(1.0, geo.max_delay);
        self.fade_pos = 0.0;
        self.fade_len = fade.max(1.0);
        self.rho = rho;
    }
}

/// The jump (samples) whose landing point best matches the playing head at
/// delay `from`, and that match's normalised correlation (0..1).
///
/// A coarse pass on the low-passed copy picks the region, a fine pass on the
/// full-band signal places it. Bounded: about `span / coarse_step` coarse
/// candidates and `2 * coarse_step / refine_step` fine ones, each over a
/// fixed number of window points, at any sample rate.
fn best_jump(
    ring: &Ring,
    from: f32,
    up: bool,
    jump_lo: f32,
    jump_hi: f32,
    geo: &Geometry,
) -> (f32, f32) {
    let from_i = (from.round() as usize).max(1);
    let lo = (jump_lo.round() as usize).max(1);
    let hi = (jump_hi.round() as usize).max(lo);
    let middle = (lo + hi) / 2;
    // The window leans forward as far as the newest head has signal for.
    let newest = if up {
        from_i
    } else {
        from_i.saturating_sub(hi)
    };
    let fwd = (geo.match_len / 2).min(newest.saturating_sub(1));
    let back = geo.match_len - fwd;
    let a0 = ring.write.wrapping_sub(from_i).wrapping_sub(back);
    let landing = |j: usize| {
        if up {
            a0.wrapping_sub(j)
        } else {
            a0.wrapping_add(j)
        }
    };
    // Among near-equal matches the longest jump wins: a steady note's period
    // repeats across the range, and the longest repeat splices least often.
    let reach = |j: usize| LONG_JUMP_BIAS * (j - lo) as f32 / (hi - lo).max(1) as f32;

    let coarse = geo.coarse_step;
    let len = geo.match_len;
    let ana = |i: usize| ring.ana[i & ring.mask];
    let aa = window_energy(ana, a0, len, coarse);
    if aa <= 1.0e-12 {
        // Silence: any landing is as good; stay in the middle of the range.
        return (middle as f32, 0.0);
    }
    let mut best = middle;
    let mut best_score = f32::NEG_INFINITY;
    let mut j = lo;
    while j <= hi {
        let score = correlation(ana, a0, landing(j), len, coarse, aa) + reach(j);
        if score > best_score {
            best_score = score;
            best = j;
        }
        j += coarse;
    }

    let fine = geo.fine_stride;
    let mono = |i: usize| ring.mono(i);
    let aa = window_energy(mono, a0, len, fine).max(1.0e-12);
    let first = best.saturating_sub(coarse - 1).max(lo);
    let last = (best + coarse - 1).min(hi);
    let mut refined = best;
    let mut refined_score = f32::NEG_INFINITY;
    let mut j = first;
    while j <= last {
        let score = correlation(mono, a0, landing(j), len, fine, aa);
        if score > refined_score {
            refined_score = score;
            refined = j;
        }
        j += geo.refine_step;
    }
    (refined as f32, refined_score.clamp(0.0, 1.0))
}

#[inline]
fn window_energy(signal: impl Fn(usize) -> f32, start: usize, len: usize, stride: usize) -> f32 {
    let mut sum = 0.0f32;
    let mut k = 0;
    while k < len {
        let x = signal(start.wrapping_add(k));
        sum += x * x;
        k += stride;
    }
    sum
}

/// Normalised cross-correlation of the windows at `a` and `b`; `aa` is the
/// energy of the `a` window at the same stride.
#[inline]
fn correlation(
    signal: impl Fn(usize) -> f32,
    a: usize,
    b: usize,
    len: usize,
    stride: usize,
    aa: f32,
) -> f32 {
    let mut ab = 0.0f32;
    let mut bb = 0.0f32;
    let mut k = 0;
    while k < len {
        let x = signal(a.wrapping_add(k));
        let y = signal(b.wrapping_add(k));
        ab += x * y;
        bb += y * y;
        k += stride;
    }
    ab / (aa * bb + 1.0e-20).sqrt()
}

pub(super) struct PitchShift {
    sample_rate: f32,
    left: Vec<f32>,
    right: Vec<f32>,
    /// Low-passed mono input: what the splice search compares.
    ana: Vec<f32>,
    mask: usize,
    write: usize,
    geo: Geometry,
    lp_coeff: f32,
    lp1: f32,
    lp2: f32,
    /// The ratio the heads move at, gliding to the requested one.
    ratio: f32,
    glide: f32,
    blend: f32,
    blend_step: f32,
    /// Weight of the detune's second voice, so switching it fades.
    dual_gain: f32,
    voice_a: Voice,
    voice_b: Voice,
}

impl PitchShift {
    pub(super) fn new(sample_rate: f32) -> Self {
        let mut shift = Self {
            sample_rate: 1.0,
            left: Vec::new(),
            right: Vec::new(),
            ana: Vec::new(),
            mask: 0,
            write: 0,
            geo: Geometry::new(48_000.0),
            lp_coeff: 1.0,
            lp1: 0.0,
            lp2: 0.0,
            ratio: 1.0,
            glide: 1.0,
            blend: 0.0,
            blend_step: 1.0,
            dual_gain: 0.0,
            voice_a: Voice::parked(1.0),
            voice_b: Voice::parked(1.0),
        };
        shift.set_sample_rate(sample_rate);
        shift
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        self.geo = Geometry::new(sr);
        let len = self.geo.ring;
        self.left = vec![0.0; len];
        self.right = vec![0.0; len];
        self.ana = vec![0.0; len];
        self.mask = len - 1;
        self.lp_coeff = 1.0 - (-std::f32::consts::TAU * ANALYSIS_LOWPASS_HZ / sr).exp();
        self.glide = 1.0 - (-1.0 / (sr * GLIDE_SECONDS).max(1.0)).exp();
        self.blend_step = 1.0 / (sr * BLEND_SECONDS).max(1.0);
        self.reset();
    }

    pub(super) fn reset(&mut self) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        self.ana.fill(0.0);
        self.write = 0;
        self.lp1 = 0.0;
        self.lp2 = 0.0;
        self.ratio = 1.0;
        self.blend = 0.0;
        self.dual_gain = 0.0;
        self.voice_a = Voice::parked(self.geo.min_delay);
        self.voice_b = Voice::parked(self.geo.min_delay);
    }

    /// Samples of delay a shifted signal carries once the blend has opened.
    /// The host should use [`Self::delay_samples`] together with whether the
    /// shift is engaged: the blend is still closed on the control-thread
    /// edit that turns a transpose on.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn latency_samples(&self) -> usize {
        if self.blend < 1.0e-3 {
            0
        } else {
            self.delay_samples()
        }
    }

    /// Delay a shifted signal carries, whether or not the blend has opened.
    /// The same at every ratio, so a shift edit never moves it.
    pub(super) fn delay_samples(&self) -> usize {
        (self.sample_rate * LATENCY_SECONDS).round() as usize
    }

    /// `ratio` is the pitch multiplier (2.0 is an octave up). `dual` runs a
    /// second voice at the reciprocal ratio and averages the two: the detune
    /// mode. Unity with `dual` clear returns the input.
    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32, ratio: f32, dual: bool) -> (f32, f32) {
        let target = if ratio.is_finite() {
            ratio.clamp(MIN_RATIO, MAX_RATIO)
        } else {
            1.0
        };
        let unity = !dual && (target - 1.0).abs() < 1.0e-4;
        let write = self.write;
        let mask = self.mask;
        self.left[write] = left;
        self.right[write] = right;
        let mono = 0.5 * (left + right);
        self.lp1 = flush_denormal(self.lp1 + (mono - self.lp1) * self.lp_coeff);
        self.lp2 = flush_denormal(self.lp2 + (self.lp1 - self.lp2) * self.lp_coeff);
        self.ana[write] = self.lp2;
        self.write = (write + 1) & mask;

        if self.blend <= 0.0 {
            if unity {
                return (left, right);
            }
            // Engaging from dry: nothing is audible yet, so start at the
            // requested ratio from a short delay.
            self.ratio = target;
            self.voice_a = Voice::parked(self.geo.start_delay(target));
            self.voice_b = Voice::parked(self.geo.start_delay(1.0 / target));
            self.dual_gain = if dual { 1.0 } else { 0.0 };
        } else {
            let gap = target - self.ratio;
            self.ratio = if gap.abs() < 1.0e-6 {
                target
            } else {
                self.ratio + gap * self.glide
            };
        }
        let ratio = self.ratio;
        let geo = self.geo;
        let ring = Ring {
            left: &self.left,
            right: &self.right,
            ana: &self.ana,
            mask,
            write,
        };

        let (mut wet_l, mut wet_r) = self.voice_a.tick(&ring, ratio, &geo);
        let dual_target = if dual { 1.0 } else { 0.0 };
        if dual && self.dual_gain <= 0.0 {
            // The second voice joins from where the first one is, so it
            // starts as a copy and drifts apart.
            self.voice_b = self.voice_a;
        }
        self.dual_gain += (dual_target - self.dual_gain).clamp(-self.blend_step, self.blend_step);
        if self.dual_gain > 0.0 {
            let (b_l, b_r) = self.voice_b.tick(&ring, 1.0 / ratio, &geo);
            let g = self.dual_gain;
            let norm = 1.0 / (1.0 + g);
            wet_l = (wet_l + b_l * g) * norm;
            wet_r = (wet_r + b_r * g) * norm;
        }

        let blend_target = if unity { 0.0 } else { 1.0 };
        self.blend += (blend_target - self.blend).clamp(-self.blend_step, self.blend_step);
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

/// Linear read `delay` samples behind the sample at `write` (which the caller
/// has just written): a delay of 1 is the previous input.
#[inline]
fn read_at(buf: &[f32], mask: usize, write: usize, delay: f32) -> f32 {
    let delay = delay.max(1.0);
    let whole = delay.floor();
    let frac = delay - whole;
    let i0 = write.wrapping_sub(whole as usize) & mask;
    let i1 = i0.wrapping_sub(1) & mask;
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

    fn sine(n: usize, hz: f32, sr: f32) -> f32 {
        (n as f32 * std::f32::consts::TAU * hz / sr).sin()
    }

    fn shifted(input: &[f32], sr: f32, ratio: f32, dual: bool) -> Vec<f32> {
        let mut shift = PitchShift::new(sr);
        input
            .iter()
            .map(|&x| shift.process(x, x, ratio, dual).0)
            .collect()
    }

    /// The shifter this one replaced: two heads on a fixed 40 ms grain with
    /// blind equal-power sine windows. Kept here only as the yardstick for
    /// the quality tests.
    fn legacy_shifted(input: &[f32], sr: f32, ratio: f32) -> Vec<f32> {
        let window = (sr * 0.04).round();
        let len = (window as usize * 8).next_power_of_two();
        let mask = len - 1;
        let mut buf = vec![0.0f32; len];
        let mut phase = 0.25f32;
        let mut out = Vec::with_capacity(input.len());
        for (write, &x) in input.iter().enumerate() {
            let write = write & mask;
            buf[write] = x;
            let other = (phase + 0.5).rem_euclid(1.0);
            let a = read_at(&buf, mask, write, phase * window);
            let b = read_at(&buf, mask, write, other * window);
            let w_a = (std::f32::consts::PI * phase).sin();
            let w_b = (std::f32::consts::PI * other).sin();
            out.push(a * w_a + b * w_b);
            phase = (phase + (1.0 - ratio) / window).rem_euclid(1.0);
        }
        out
    }

    /// Coefficient of variation of 20 ms RMS frames over the settled second
    /// half: how much the level pumps.
    fn envelope_ripple(samples: &[f32], sr: f32) -> f32 {
        let frame = (sr * 0.02) as usize;
        let start = samples.len() / 2;
        let rms: Vec<f32> = samples[start..]
            .chunks_exact(frame)
            .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / frame as f32).sqrt())
            .collect();
        let mean = rms.iter().sum::<f32>() / rms.len() as f32;
        let var = rms.iter().map(|r| (r - mean).powi(2)).sum::<f32>() / rms.len() as f32;
        var.sqrt() / mean.max(1.0e-9)
    }

    /// Share of the settled output (second half, 100 ms frames) that a
    /// least-squares fit of steady sinusoids at `freqs` does not explain, in
    /// dB. A clean shift is the input's partials at the new pitch with a
    /// fixed amplitude and phase per frame; splice warble, phase jumps and
    /// level pumping all land in the residual.
    fn residual_db(samples: &[f32], sr: f32, freqs: &[f32]) -> f32 {
        let frame = (sr * 0.1) as usize;
        let start = samples.len() / 2;
        let k = freqs.len() * 2;
        let (mut resid, mut total) = (0.0f64, 0.0f64);
        for chunk in samples[start..].chunks_exact(frame) {
            let basis = |i: usize, b: usize| {
                let w = std::f64::consts::TAU * f64::from(freqs[b / 2]) / f64::from(sr);
                if b % 2 == 0 {
                    (w * i as f64).cos()
                } else {
                    (w * i as f64).sin()
                }
            };
            let mut g = vec![vec![0.0f64; k + 1]; k];
            for (i, &x) in chunk.iter().enumerate() {
                let phi: Vec<f64> = (0..k).map(|b| basis(i, b)).collect();
                for r in 0..k {
                    for c in 0..k {
                        g[r][c] += phi[r] * phi[c];
                    }
                    g[r][k] += phi[r] * f64::from(x);
                }
            }
            // Gaussian elimination with partial pivoting.
            for col in 0..k {
                let pivot = (col..k)
                    .max_by(|&a, &b| g[a][col].abs().total_cmp(&g[b][col].abs()))
                    .unwrap();
                g.swap(col, pivot);
                for row in 0..k {
                    if row != col {
                        let f = g[row][col] / g[col][col];
                        for c in col..=k {
                            g[row][c] -= f * g[col][c];
                        }
                    }
                }
            }
            let coef: Vec<f64> = (0..k).map(|r| g[r][k] / g[r][r]).collect();
            for (i, &x) in chunk.iter().enumerate() {
                let fit: f64 = (0..k).map(|b| coef[b] * basis(i, b)).sum();
                resid += (f64::from(x) - fit).powi(2);
                total += f64::from(x).powi(2);
            }
        }
        (10.0 * (resid / total.max(1.0e-30)).log10()) as f32
    }

    /// A guitar-ish note: fundamental and decaying harmonics, steady.
    fn note(n: usize, hz: f32, sr: f32) -> f32 {
        (1..=6)
            .map(|h| sine(n, hz * h as f32, sr) / h as f32)
            .sum::<f32>()
            * 0.2
    }

    /// An open A-major-ish chord (A2 E3 A3 C#4 E4), each a `note`.
    fn chord(n: usize, sr: f32) -> f32 {
        [110.0, 164.81, 220.0, 277.18, 329.63]
            .iter()
            .map(|&hz| note(n, hz, sr))
            .sum::<f32>()
            * 0.4
    }

    /// Equal-tempered voicings: A major (A2 E3 A3 C#4 E4), C major (C3 E3
    /// G3 C4 E4) and E7 (E2 B2 D3 G#3 B3 E4). Plain tones, so the fit in
    /// [`residual_db`] has one partial per tone.
    const CHORDS: [&[f32]; 3] = [
        &[110.0, 164.81, 220.0, 277.18, 329.63],
        &[130.81, 164.81, 196.0, 261.63, 329.63],
        &[82.41, 123.47, 146.83, 207.65, 246.94, 329.63],
    ];

    const SHIFTS: [f32; 6] = [12.0, 7.0, 3.0, -1.0, -5.0, -12.0];

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
        let input: Vec<f32> = (0..16_000).map(|n| sine(n, 220.0, sr) * 0.5).collect();
        let hz = dominant_hz(&shifted(&input, sr, 2.0, false), sr);
        assert!(
            (hz - 440.0).abs() < 10.0,
            "octave up read {hz} Hz, expected near 440"
        );
    }

    #[test]
    fn an_octave_down_halves_the_rate() {
        let sr = 48_000.0;
        let input: Vec<f32> = (0..24_000).map(|n| sine(n, 440.0, sr) * 0.5).collect();
        let hz = dominant_hz(&shifted(&input, sr, 0.5, false), sr);
        assert!(
            (hz - 220.0).abs() < 6.0,
            "octave down read {hz} Hz, expected near 220"
        );
    }

    #[test]
    fn a_fifth_up_lands_on_the_fifth() {
        let sr = 48_000.0;
        let ratio = semitone_ratio(7.0);
        let input: Vec<f32> = (0..24_000).map(|n| sine(n, 220.0, sr) * 0.5).collect();
        let hz = dominant_hz(&shifted(&input, sr, ratio, false), sr);
        let expected = 220.0 * ratio;
        assert!(
            (hz - expected).abs() < 6.0,
            "fifth read {hz} Hz, expected near {expected}"
        );
    }

    #[test]
    fn a_shifted_signal_reports_its_delay_once_open() {
        let mut shift = PitchShift::new(48_000.0);
        for n in 0..4_800 {
            shift.process(sine(n, 220.0, 48_000.0), 0.0, 2.0, false);
        }
        assert_eq!(shift.latency_samples(), shift.delay_samples());
        // Back to unity: the blend closes and the delay goes with it.
        for n in 0..4_800 {
            shift.process(sine(n, 220.0, 48_000.0), 0.0, 1.0, false);
        }
        assert_eq!(shift.latency_samples(), 0);
    }

    #[test]
    fn a_shifted_note_stays_steady() {
        // The "robotic" sound: a blind splice lands mid-cycle, so the
        // shifted note's phase hops and its level dips at the grain rate.
        // Matched splices leave a steady note at the new pitch.
        let sr = 48_000.0;
        let tone: Vec<f32> = (0..72_000).map(|n| sine(n, 220.0, sr) * 0.5).collect();
        let notes: Vec<f32> = (0..72_000).map(|n| note(n, 110.0, sr)).collect();
        for semis in SHIFTS {
            let r = semitone_ratio(semis);
            let note_hz: Vec<f32> = (1..=6).map(|h| 110.0 * r * h as f32).collect();
            for (name, input, hz) in [
                ("sine", &tone, &[220.0 * r][..]),
                ("note", &notes, &note_hz[..]),
            ] {
                let new = residual_db(&shifted(input, sr, r, false), sr, hz);
                let old = residual_db(&legacy_shifted(input, sr, r), sr, hz);
                println!("{name} {semis:+} st: residual {new:.1} dB (old design {old:.1} dB)");
                assert!(new < -25.0, "{name} {semis:+} st: residual {new} dB");
                assert!(
                    new < old - 10.0,
                    "{name} {semis:+} st: {new} dB vs old {old} dB"
                );
            }
        }
    }

    #[test]
    fn a_shifted_chord_keeps_its_level() {
        // No single period for a chord to splice on. What must hold: no
        // deep periodic dips (20 ms RMS ripple stays bounded and does not
        // get much worse than the old design's), and a chord whose tones
        // share a period within reach is spliced cleanly.
        let sr = 48_000.0;
        for (i, tones) in CHORDS.iter().enumerate() {
            let input: Vec<f32> = (0..72_000)
                .map(|n| tones.iter().map(|&hz| sine(n, hz, sr)).sum::<f32>() * 0.15)
                .collect();
            for semis in SHIFTS {
                let r = semitone_ratio(semis);
                let new_out = shifted(&input, sr, r, false);
                let old_out = legacy_shifted(&input, sr, r);
                let new = envelope_ripple(&new_out, sr);
                let old = envelope_ripple(&old_out, sr);
                let hz: Vec<f32> = tones.iter().map(|f| f * r).collect();
                let fit_new = residual_db(&new_out, sr, &hz);
                let fit_old = residual_db(&old_out, sr, &hz);
                println!(
                    "chord {i} {semis:+} st: ripple {new:.3} (old {old:.3}), residual {fit_new:.1} dB (old {fit_old:.1} dB)"
                );
                assert!(new < 0.25, "chord {i} {semis:+} st: ripple {new}");
                assert!(
                    new < old * 1.9,
                    "chord {i} {semis:+} st: {new} vs old {old}"
                );
                if i == 0 {
                    // A major's tones sit on a 55 Hz grid: one 18 ms period.
                    assert!(fit_new < -12.0, "A major {semis:+} st: {fit_new} dB");
                }
            }
        }
    }

    /// Largest step between neighbouring output samples.
    fn max_step(samples: &[f32]) -> f32 {
        samples
            .windows(2)
            .map(|p| (p[1] - p[0]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn a_ratio_sweep_glides_without_clicks() {
        let sr = 48_000.0;
        let mut shift = PitchShift::new(sr);
        let total = 96_000;
        let mut out = Vec::with_capacity(total);
        for n in 0..total {
            // 0.5 -> 2.0 and back, exponentially, plus hard steps.
            let t = n as f32 / total as f32;
            let sweep =
                (std::f32::consts::LN_2 * (2.0 * (std::f32::consts::PI * t).sin() - 1.0)).exp();
            let ratio = match n {
                30_000..=40_000 => 1.6,
                40_001..=50_000 => 0.7,
                _ => sweep,
            };
            out.push(shift.process(sine(n, 220.0, sr) * 0.5, 0.0, ratio, false).0);
        }
        // The fastest a 0.5 sine at the highest output pitch (440 Hz) moves.
        let limit = 0.5 * std::f32::consts::TAU * 440.0 / sr;
        let step = max_step(&out[2_000..]);
        assert!(
            step < limit * 1.6,
            "sweep clicked: step {step}, sine slope {limit}"
        );
    }

    #[test]
    fn detune_switching_and_engaging_do_not_click() {
        let sr = 48_000.0;
        let mut shift = PitchShift::new(sr);
        let mut out = Vec::new();
        for n in 0..48_000 {
            let (ratio, dual) = match n {
                0..=9_999 => (1.0, false),
                10_000..=19_999 => (semitone_ratio(0.15), true),
                20_000..=29_999 => (semitone_ratio(0.15), false),
                30_000..=39_999 => (semitone_ratio(-3.0), false),
                _ => (1.0, false),
            };
            out.push(shift.process(sine(n, 196.0, sr) * 0.5, 0.0, ratio, dual).0);
        }
        let limit = 0.5 * std::f32::consts::TAU * 200.0 / sr;
        let step = max_step(&out);
        assert!(
            step < limit * 2.0,
            "switch clicked: step {step}, slope {limit}"
        );
    }

    #[test]
    fn the_heads_average_near_the_reported_delay() {
        let sr = 48_000.0;
        for semis in [-12.0, -5.0, -1.0, 1.0, 5.0, 7.0, 12.0] {
            let ratio = semitone_ratio(semis);
            let mut shift = PitchShift::new(sr);
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for n in 0..96_000 {
                let x = if semis < 0.0 {
                    chord(n, sr)
                } else {
                    note(n, 98.0, sr)
                };
                shift.process(x, x, ratio, false);
                if n > 9_600 {
                    sum += f64::from(shift.voice_a.delay[shift.voice_a.active]);
                    count += 1;
                }
            }
            let mean_ms = sum / count as f64 / f64::from(sr) * 1000.0;
            let reported_ms = shift.delay_samples() as f64 / f64::from(sr) * 1000.0;
            println!("{semis:+} st: mean head delay {mean_ms:.2} ms, reported {reported_ms:.2} ms");
            assert!(
                (mean_ms - reported_ms).abs() < 6.5,
                "{semis:+} st: heads average {mean_ms} ms, reported {reported_ms} ms"
            );
        }
    }

    #[test]
    fn output_is_finite_at_every_rate() {
        for sr in [44_100.0f32, 48_000.0, 96_000.0, 192_000.0] {
            for (ratio, dual) in [
                (0.25, false),
                (0.5, false),
                (0.749, false),
                (semitone_ratio(0.2), true),
                (1.06, false),
                (1.5, false),
                (2.0, false),
                (4.0, false),
            ] {
                let mut shift = PitchShift::new(sr);
                let mut seed = 0x1234_5678u32;
                let mut peak = 0.0f32;
                for n in 0..(sr as usize / 2) {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                    let x = sine(n, 98.0, sr) * 0.5 + noise * 0.3;
                    let (l, r) = shift.process(x, -x, ratio, dual);
                    assert!(l.is_finite() && r.is_finite(), "{sr} Hz ratio {ratio}");
                    // The geometry leaves every head room: none rides a clamp.
                    let d = shift.voice_a.delay[shift.voice_a.active];
                    assert!(
                        d > 1.5 && d < shift.geo.max_delay - 1.0,
                        "{sr} Hz ratio {ratio}: head at {d}"
                    );
                    peak = peak.max(l.abs()).max(r.abs());
                }
                assert!(peak < 2.0, "{sr} Hz ratio {ratio}: peak {peak}");
                assert!(peak > 0.1, "{sr} Hz ratio {ratio}: silent");
            }
        }
    }

    #[test]
    fn processing_keeps_the_buffers_it_was_given() {
        // `process` must not grow or replace a buffer: the same allocation
        // and length after a run through every kind of splice.
        let mut shift = PitchShift::new(48_000.0);
        let before = (
            shift.left.as_ptr(),
            shift.right.as_ptr(),
            shift.ana.as_ptr(),
            shift.left.len(),
        );
        for n in 0..48_000 {
            let ratio = [0.5, 0.94, 1.0, 1.5, 2.0][n / 10_000];
            shift.process(chord(n, 48_000.0), 0.0, ratio, n % 20_000 < 5_000);
        }
        shift.reset();
        let after = (
            shift.left.as_ptr(),
            shift.right.as_ptr(),
            shift.ana.as_ptr(),
            shift.left.len(),
        );
        assert_eq!(before, after);
    }

    #[test]
    fn the_reported_delay_is_shorter_than_before() {
        // The old fixed grain carried 20 ms; this one reports 12 ms at
        // every rate.
        for sr in [44_100.0f32, 48_000.0, 96_000.0, 192_000.0] {
            let shift = PitchShift::new(sr);
            let ms = shift.delay_samples() as f32 / sr * 1000.0;
            assert!((ms - 12.0).abs() < 0.1, "{sr} Hz: {ms} ms");
        }
    }
}

//! Studio reverb — a Freeverb-style bank of parallel comb filters into series
//! all-pass diffusers (the classic Schroeder/Moorer topology popularised by
//! Jezar's public-domain Freeverb), fronted by a fractional predelay and, for
//! the Shimmer voicing, an octave-up granular pitch shifter folded into the
//! comb feedback. Four voicings — Plate, Room, Hall, Shimmer — share one
//! preallocated buffer set; a model change only re-targets smoothed voicing
//! parameters (damping, loop gain, predelay tap, shimmer amount), never
//! reallocates, so it is safe from the audio-producer thread.
//!
//! All buffers are allocated in [`PlateReverb::new`] / `rebuild` (control
//! thread). [`PlateReverb::process`] performs no allocation or locking.

use builtin_dsp_core::mix;

use super::ReverbModel;
use super::smooth::Smoothed;

/// Glide time for decay/mix/voicing edits (see `smooth.rs`).
const SMOOTH_SECONDS: f32 = 0.010;
/// Longer glide for predelay so a model swap slides the tap instead of clicking.
const PREDELAY_SMOOTH_SECONDS: f32 = 0.060;

// Freeverb tunings in samples at 44.1 kHz; scaled to the runtime rate. Hall and
// Shimmer read the buffers at a longer effective size via a scale tap (see
// `size` in the voicing), so the buffers are allocated at the largest scale.
const COMB_TUNINGS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNINGS: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;

/// Comb buffers are allocated at this multiple of the base tuning so the larger
/// voicings (Hall/Shimmer) have real modal length to read into.
const SIZE_HEADROOM: f32 = 1.6;
/// Longest predelay any voicing asks for, in seconds — sizes the predelay line.
const MAX_PREDELAY_S: f32 = 0.060;

const FIXED_GAIN: f32 = 0.015;
/// Loop gain of the shimmer regeneration at 100 % shimmer, independent of
/// decay. Below unity so the octave voice sustains and fades, never grows.
const SHIMMER_LOOP_GAIN: f32 = 0.85;
const SCALE_DAMP: f32 = 0.4;
/// Per-voicing tuning. A model switch re-targets these; nothing reallocates.
#[derive(Debug, Clone, Copy)]
struct Voicing {
    /// Predelay before the reverb, in seconds.
    predelay_s: f32,
    /// Fraction of the allocated comb length actually used (0..1]. Smaller =
    /// tighter, faster echo density (Room); larger = longer, more spacious
    /// (Hall/Shimmer).
    size: f32,
    /// Loop-gain range the decay knob maps into: `offset + amount * scale`.
    fb_offset: f32,
    fb_scale: f32,
    /// Comb damping (HF loss per pass): darker tails at higher values.
    damp: f32,
    /// Stereo input folded toward mono before entering the late field.
    crossfeed: f32,
    /// Series-allpass regeneration; higher values produce a denser tail.
    diffusion: f32,
    /// Level of the explicit early-reflection tap cluster.
    early_gain: f32,
}

impl Voicing {
    fn for_model(model: ReverbModel) -> Self {
        match model {
            // Bright, dense, immediate — the original plate.
            ReverbModel::Plate => Self {
                predelay_s: 0.0,
                size: 0.72,
                fb_offset: 0.70,
                fb_scale: 0.28,
                damp: 0.28,
                crossfeed: 0.30,
                diffusion: 0.68,
                early_gain: 0.08,
            },
            // Short, damped, early — a tight tracking room.
            ReverbModel::Room => Self {
                predelay_s: 0.008,
                size: 0.55,
                fb_offset: 0.64,
                fb_scale: 0.24,
                damp: 0.55,
                crossfeed: 0.08,
                diffusion: 0.42,
                early_gain: 0.38,
            },
            // Long predelay, long tail, gentle damping — a large hall.
            ReverbModel::Hall => Self {
                predelay_s: 0.028,
                size: 1.0,
                fb_offset: 0.74,
                fb_scale: 0.245,
                damp: 0.36,
                crossfeed: 0.42,
                diffusion: 0.58,
                early_gain: 0.18,
            },
            // Hall-sized bed with an octave-up voice regenerating in the tail.
            ReverbModel::Shimmer => Self {
                predelay_s: 0.020,
                size: 0.94,
                fb_offset: 0.74,
                fb_scale: 0.235,
                damp: 0.40,
                crossfeed: 0.34,
                diffusion: 0.62,
                early_gain: 0.14,
            },
        }
    }
}

/// Fractional delay line with linear-interpolated taps, for the predelay and
/// the shimmer pitch shifter. Preallocated; reads never allocate.
#[derive(Debug, Clone)]
struct DelayLine {
    buffer: Vec<f32>,
    write: usize,
}

impl DelayLine {
    fn new(len: usize) -> Self {
        Self {
            buffer: vec![0.0; len.max(1)],
            write: 0,
        }
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.buffer[self.write] = x;
        self.write += 1;
        if self.write >= self.buffer.len() {
            self.write = 0;
        }
    }

    /// Read `delay` samples behind the write head (fractional, linear interp).
    #[inline]
    fn read(&self, delay: f32) -> f32 {
        let len = self.buffer.len();
        let d = delay.clamp(1.0, (len - 1) as f32);
        let base = d.floor();
        let frac = d - base;
        let i0 = (self.write + len - base as usize) % len;
        let i1 = (i0 + len - 1) % len;
        self.buffer[i0] * (1.0 - frac) + self.buffer[i1] * frac
    }
}

/// Octave-up granular pitch shifter: two delay taps sweeping in antiphase with
/// a sinusoidal crossfade (the Bode/Dattorro two-grain shifter). Feeds the
/// shimmer voicing's regenerating tail. Allocation-free in `process`.
#[derive(Debug, Clone)]
struct OctaveUp {
    line: DelayLine,
    window: f32,
    phase: f32,
    inc: f32,
}

impl OctaveUp {
    fn new(sample_rate: f32) -> Self {
        // ~50 ms grain window: long enough that the octave transposition reads
        // smoothly, short enough that the tail stays tight.
        let window = (sample_rate * 0.050).max(2.0);
        Self {
            line: DelayLine::new(window as usize + 4),
            window,
            phase: 0.0,
            // Octave up (ratio 2): the read delay sweeps one window per grain,
            // so the read head advances at twice the write rate.
            inc: 1.0 / window,
        }
    }

    fn clear(&mut self) {
        self.line.clear();
        self.phase = 0.0;
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        self.line.push(x);
        self.phase -= self.inc;
        if self.phase < 0.0 {
            self.phase += 1.0;
        }
        let p2 = if self.phase + 0.5 >= 1.0 {
            self.phase - 0.5
        } else {
            self.phase + 0.5
        };
        let d1 = self.phase * self.window + 1.0;
        let d2 = p2 * self.window + 1.0;
        // sin() crossfade is constant-power across the grain overlap.
        let g1 = (std::f32::consts::PI * self.phase).sin();
        let g2 = (std::f32::consts::PI * p2).sin();
        self.line.read(d1) * g1 + self.line.read(d2) * g2
    }
}

#[derive(Debug, Clone)]
struct Comb {
    buffer: Vec<f32>,
    write: usize,
    filter_store: f32,
}

impl Comb {
    fn new(size: usize) -> Self {
        Self {
            buffer: vec![0.0; size.max(1)],
            write: 0,
            filter_store: 0.0,
        }
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.filter_store = 0.0;
        self.write = 0;
    }

    /// `read_len` selects the effective loop length (≤ allocated), so the
    /// voicing can trade modal density without reallocating. `feedback` and the
    /// damping coefficients are passed per sample so decay/voicing edits glide.
    #[inline]
    fn process(
        &mut self,
        input: f32,
        read_len: usize,
        feedback: f32,
        damp1: f32,
        damp2: f32,
    ) -> f32 {
        let len = self.buffer.len();
        let read = (self.write + len - read_len.clamp(1, len)) % len;
        let output = self.buffer[read];
        self.filter_store = output * damp2 + self.filter_store * damp1;
        // Flush denormals to keep the tail cheap.
        if self.filter_store.abs() < 1.0e-18 {
            self.filter_store = 0.0;
        }
        self.buffer[self.write] = input + self.filter_store * feedback;
        self.write += 1;
        if self.write >= len {
            self.write = 0;
        }
        output
    }
}

#[derive(Debug, Clone)]
struct Allpass {
    buffer: Vec<f32>,
    index: usize,
}

impl Allpass {
    fn new(size: usize) -> Self {
        Self {
            buffer: vec![0.0; size.max(1)],
            index: 0,
        }
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.index = 0;
    }

    #[inline]
    fn process(&mut self, input: f32, feedback: f32) -> f32 {
        let buffered = self.buffer[self.index];
        let output = -input + buffered;
        self.buffer[self.index] = input + buffered * feedback;
        self.index += 1;
        if self.index >= self.buffer.len() {
            self.index = 0;
        }
        output
    }
}

#[derive(Debug, Clone)]
pub(super) struct PlateReverb {
    sample_rate: f32,
    combs_l: Vec<Comb>,
    combs_r: Vec<Comb>,
    allpass_l: Vec<Allpass>,
    allpass_r: Vec<Allpass>,
    /// Base (unscaled, sample-rate-adjusted) comb read lengths, one per comb.
    comb_base_l: Vec<usize>,
    comb_base_r: Vec<usize>,
    predelay_l: DelayLine,
    predelay_r: DelayLine,
    shimmer_l: OctaveUp,
    shimmer_r: OctaveUp,
    feedback: Smoothed,
    mix: Smoothed,
    damp: Smoothed,
    size: Smoothed,
    crossfeed: Smoothed,
    diffusion: Smoothed,
    early_gain: Smoothed,
    shimmer: Smoothed,
    predelay_samples: Smoothed,
    /// Octave-up energy carried into the next block's comb input, so the
    /// transposed voice regenerates. Bounded to keep the shimmer loop stable.
    shimmer_fb_l: f32,
    shimmer_fb_r: f32,
}

impl PlateReverb {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        let base = Voicing::for_model(ReverbModel::Plate);
        let mut reverb = Self {
            sample_rate: sr,
            combs_l: Vec::new(),
            combs_r: Vec::new(),
            allpass_l: Vec::new(),
            allpass_r: Vec::new(),
            comb_base_l: Vec::new(),
            comb_base_r: Vec::new(),
            predelay_l: DelayLine::new(1),
            predelay_r: DelayLine::new(1),
            shimmer_l: OctaveUp::new(sr),
            shimmer_r: OctaveUp::new(sr),
            feedback: Smoothed::new(sr, SMOOTH_SECONDS, base.fb_offset),
            mix: Smoothed::new(sr, SMOOTH_SECONDS, 0.55),
            damp: Smoothed::new(sr, SMOOTH_SECONDS, base.damp),
            size: Smoothed::new(sr, SMOOTH_SECONDS, base.size),
            crossfeed: Smoothed::new(sr, SMOOTH_SECONDS, base.crossfeed),
            diffusion: Smoothed::new(sr, SMOOTH_SECONDS, base.diffusion),
            early_gain: Smoothed::new(sr, SMOOTH_SECONDS, base.early_gain),
            shimmer: Smoothed::new(sr, SMOOTH_SECONDS, 0.0),
            predelay_samples: Smoothed::new(sr, PREDELAY_SMOOTH_SECONDS, 1.0),
            shimmer_fb_l: 0.0,
            shimmer_fb_r: 0.0,
        };
        reverb.rebuild();
        reverb
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.feedback.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.mix.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.damp.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.size.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.crossfeed.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.diffusion.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.early_gain.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.shimmer.set_time(self.sample_rate, SMOOTH_SECONDS);
        self.predelay_samples
            .set_time(self.sample_rate, PREDELAY_SMOOTH_SECONDS);
        self.shimmer_l = OctaveUp::new(self.sample_rate);
        self.shimmer_r = OctaveUp::new(self.sample_rate);
        self.rebuild();
    }

    pub(super) fn reset(&mut self) {
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            c.clear();
        }
        for a in self.allpass_l.iter_mut().chain(self.allpass_r.iter_mut()) {
            a.clear();
        }
        self.predelay_l.clear();
        self.predelay_r.clear();
        self.shimmer_l.clear();
        self.shimmer_r.clear();
        self.shimmer_fb_l = 0.0;
        self.shimmer_fb_r = 0.0;
        self.feedback.snap();
        self.mix.snap();
        self.damp.snap();
        self.size.snap();
        self.crossfeed.snap();
        self.diffusion.snap();
        self.early_gain.snap();
        self.shimmer.snap();
        self.predelay_samples.snap();
    }

    /// Allocate all buffers at the largest voicing size for the current sample
    /// rate. Runs on construction / sample-rate change only.
    fn rebuild(&mut self) {
        let scale = self.sample_rate / 44_100.0;
        // Allocate with size headroom so Hall/Shimmer have real length to read.
        let alloc = |len: usize| ((len as f32 * scale * SIZE_HEADROOM) as usize).max(1);
        let base = |len: usize| ((len as f32 * scale) as usize).max(1);

        self.combs_l = COMB_TUNINGS.iter().map(|&t| Comb::new(alloc(t))).collect();
        self.combs_r = COMB_TUNINGS
            .iter()
            .map(|&t| Comb::new(alloc(t + STEREO_SPREAD)))
            .collect();
        self.comb_base_l = COMB_TUNINGS.iter().map(|&t| base(t)).collect();
        self.comb_base_r = COMB_TUNINGS
            .iter()
            .map(|&t| base(t + STEREO_SPREAD))
            .collect();
        self.allpass_l = ALLPASS_TUNINGS
            .iter()
            .map(|&t| Allpass::new(base(t)))
            .collect();
        self.allpass_r = ALLPASS_TUNINGS
            .iter()
            .map(|&t| Allpass::new(base(t + STEREO_SPREAD)))
            .collect();

        let predelay_len = (self.sample_rate * MAX_PREDELAY_S) as usize + 2;
        self.predelay_l = DelayLine::new(predelay_len);
        self.predelay_r = DelayLine::new(predelay_len);
    }

    /// `model` selects the voicing; `decay_s` is 0.5..15 and the percentage
    /// controls are 0..100. `shimmer` is ignored by non-Shimmer models.
    pub(super) fn configure(&mut self, model: ReverbModel, decay_s: f32, mix: f32, shimmer: f32) {
        let v = Voicing::for_model(model);
        let amount = ((decay_s - 0.5) / 14.5).clamp(0.0, 1.0);
        self.feedback
            .set_target((v.fb_offset + amount * v.fb_scale).clamp(0.0, 0.985));
        self.mix.set_target((mix / 100.0).clamp(0.0, 1.0));
        self.damp.set_target(v.damp);
        self.size.set_target(v.size);
        self.crossfeed.set_target(v.crossfeed);
        self.diffusion.set_target(v.diffusion);
        self.early_gain.set_target(v.early_gain);
        self.shimmer.set_target(if model == ReverbModel::Shimmer {
            (shimmer / 100.0).clamp(0.0, 1.0)
        } else {
            0.0
        });
        self.predelay_samples
            .set_target((v.predelay_s * self.sample_rate).max(1.0));
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let feedback = self.feedback.tick();
        let mix_amount = self.mix.tick();
        let damp = self.damp.tick();
        let size = self.size.tick().clamp(0.2, 1.0);
        let crossfeed = self.crossfeed.tick().clamp(0.0, 0.5);
        let diffusion = self.diffusion.tick().clamp(0.0, 0.75);
        let early_gain = self.early_gain.tick().clamp(0.0, 0.5);
        let shimmer = self.shimmer.tick();
        let predelay = self.predelay_samples.tick();

        let damp1 = damp * SCALE_DAMP;
        let damp2 = 1.0 - damp1;

        // Keep the channels independent before controlled crossfeed. Summing
        // L+R here used to erase anti-phase and wide stereo material from the
        // wet path entirely.
        self.predelay_l.push(left);
        self.predelay_r.push(right);
        let pd_l = self.predelay_l.read(predelay);
        let pd_r = self.predelay_r.read(predelay);
        let mono = (pd_l + pd_r) * 0.5;
        let input_l =
            (pd_l * (1.0 - crossfeed) + mono * crossfeed) * FIXED_GAIN + self.shimmer_fb_l;
        let input_r =
            (pd_r * (1.0 - crossfeed) + mono * crossfeed) * FIXED_GAIN + self.shimmer_fb_r;

        // A short asymmetric tap cluster gives Room a real early-reflection
        // algorithm instead of merely shortening the same late tail. The other
        // voicings retain a quieter amount for spatial definition.
        let sr = self.sample_rate;
        let early_l = self.predelay_l.read(sr * 0.0031) * 0.44
            + self.predelay_r.read(sr * 0.0053) * 0.28
            - self.predelay_l.read(sr * 0.0117) * 0.18
            + self.predelay_r.read(sr * 0.0179) * 0.10;
        let early_r = self.predelay_r.read(sr * 0.0037) * 0.44
            + self.predelay_l.read(sr * 0.0061) * 0.28
            - self.predelay_r.read(sr * 0.0131) * 0.18
            + self.predelay_l.read(sr * 0.0193) * 0.10;

        let mut wet_l = 0.0;
        for (comb, &base) in self.combs_l.iter_mut().zip(self.comb_base_l.iter()) {
            let read_len = ((base as f32 * size) as usize).max(1);
            wet_l += comb.process(input_l, read_len, feedback, damp1, damp2);
        }
        let mut wet_r = 0.0;
        for (comb, &base) in self.combs_r.iter_mut().zip(self.comb_base_r.iter()) {
            let read_len = ((base as f32 * size) as usize).max(1);
            wet_r += comb.process(input_r, read_len, feedback, damp1, damp2);
        }

        // The comb sum, before diffusion: the shimmer's tap (see below).
        let combs_l = wet_l;
        let combs_r = wet_r;
        for allpass in &mut self.allpass_l {
            wet_l = allpass.process(wet_l, diffusion);
        }
        for allpass in &mut self.allpass_r {
            wet_r = allpass.process(wet_r, diffusion);
        }
        wet_l += early_l * early_gain;
        wet_r += early_r * early_gain;

        // Shimmer: transpose the diffused tail an octave up and carry it into
        // the next sample's comb input, so the shifted voice regenerates and
        // ascends. Off (0) for every non-Shimmer voicing. The carry is bounded
        // and scaled well below unity so the second loop can only sustain, not
        // run away, on top of the comb bank's own sub-unity feedback.
        if shimmer > 1.0e-4 {
            // This is a second feedback loop around the already-regenerating
            // comb bank, so its gain has to be bounded, not just its samples:
            // with a fixed carry the loop passed unity above ~30 % shimmer and
            // the tail ran away to +40 dBFS (tanh caps each sample, not the
            // gain). The tap is the comb sum, not the diffused output: the
            // Freeverb-style diffusers are not unity-gain (up to
            // (1+g)/(1-g) per stage), while the comb sum is bounded by
            // `COMBS / (1 - feedback)` — the damping filter only lowers gain
            // above DC — and the octave shifter's crossfade by sqrt(2).
            // Dividing the carry by that bound holds the loop at or under
            // `shimmer * SHIMMER_LOOP_GAIN` at every decay: the octave voice
            // sustains and fades, never grows.
            let up_l = self.shimmer_l.process(combs_l);
            let up_r = self.shimmer_r.process(combs_r);
            let bound =
                COMB_TUNINGS.len() as f32 * std::f32::consts::SQRT_2 / (1.0 - feedback).max(1.0e-3);
            let carry = shimmer * SHIMMER_LOOP_GAIN / bound;
            let fb_l = (up_l * carry).tanh();
            let fb_r = (up_r * carry).tanh();
            self.shimmer_fb_l = if fb_l.is_finite() { fb_l } else { 0.0 };
            self.shimmer_fb_r = if fb_r.is_finite() { fb_r } else { 0.0 };
        } else {
            self.shimmer_fb_l = 0.0;
            self.shimmer_fb_r = 0.0;
        }

        (mix(left, wet_l, mix_amount), mix(right, wet_r, mix_amount))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One 0.3 s note into the wet signal, then silence: the peak, and the
    /// loudest sample of the last second.
    fn peak_and_tail(decay_s: f32, shimmer: f32) -> (f32, f32) {
        let sr = 48_000.0;
        let mut reverb = PlateReverb::new(sr);
        reverb.configure(ReverbModel::Shimmer, decay_s, 100.0, shimmer);
        reverb.reset();
        let (mut peak, mut tail) = (0.0f32, 0.0f32);
        for i in 0..(sr as usize * 8) {
            let t = i as f32 / sr;
            let x = if t < 0.3 {
                0.5 * (t * 196.0 * std::f32::consts::TAU).sin()
            } else {
                0.0
            };
            let (l, r) = reverb.process(x, x);
            peak = peak.max(l.abs()).max(r.abs());
            if t > 7.0 {
                tail = tail.max(l.abs()).max(r.abs());
            }
        }
        (peak, tail)
    }

    #[test]
    fn shimmer_never_runs_away() {
        // Every corner of the decay x shimmer range used to be checked by ear;
        // above ~30 % shimmer the regeneration loop passed unity gain and the
        // tail sat at +40 dBFS forever.
        for decay in [0.5, 4.0, 8.5, 15.0] {
            for shimmer in [30.0, 62.0, 100.0] {
                let (peak, tail) = peak_and_tail(decay, shimmer);
                assert!(peak < 1.0, "decay {decay} shimmer {shimmer}: peak {peak}");
                let (_, dry_tail) = peak_and_tail(decay, 0.0);
                assert!(
                    tail < dry_tail.max(1.0e-4) * 20.0,
                    "decay {decay} shimmer {shimmer}: tail {tail} does not decay"
                );
            }
        }
    }
}

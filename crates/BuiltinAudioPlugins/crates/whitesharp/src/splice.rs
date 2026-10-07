//! The live path: pitch shifting with no lookahead.
//!
//! The input runs into a delay line that a head reads at the shifted speed:
//! the ratio. Reading faster than the input arrives, the head creeps up on
//! the newest sample; slower, it falls behind. Before it gets too close (or
//! too far) a second head starts one period further back (or forward) and
//! the first fades out into it — a pitch-synchronous splice. A voice
//! repeats every period, so both heads read nearly the same waveform: the
//! jump is the detected period, refined to the offset whose recent
//! waveform matches best, and the fade is heard as nothing but the new
//! pitch. Splicing on the period, not on a fixed clock, is what keeps the
//! classic two-head shifter's flutter out.
//!
//! Nothing is read from the future. With the ratio at one the output is the
//! input [`LATENCY`] samples late: the interpolator's reach, and the only
//! fixed delay, which is what the plug-in reports. While the pitch is being
//! moved the head also wanders between that and about one period further
//! back (more for a large upward shift, whose fades need room); it returns
//! to the minimum once the voice stops. That part changes as the voice
//! does and cannot be compensated.
//!
//! The trade-off against the PSOLA path: a head reads whole stretches of
//! the input faster or slower, so the formants move with the pitch — there
//! is no formant correction or throat here, which need grains centred on
//! pitch marks, and so lookahead. A splice that cannot line up (a voice
//! changing shape within a period) blurs for the length of its fade.
//! Everything is sized at construction; nothing allocates per sample.

use crate::shifter::Track;

/// The fixed delay of the live path, in samples: Catmull-Rom reads two
/// samples ahead of the position it interpolates.
pub const LATENCY: usize = 2;

const MIN_DELAY: f64 = LATENCY as f64;
/// The level a created vibrato's amplitude depth sets, followed over this.
const GAIN_MS: f32 = 4.0;
/// Shortest and longest pitch-synchronous fade.
const MIN_FADE_MS: f32 = 0.25;
const MAX_FADE_MS: f32 = 10.0;
/// Once the voice has been unvoiced this long, the head returns to the
/// minimum delay over [`RESET_FADE_MS`], so each phrase starts on time.
const RESET_AFTER_MS: f32 = 12.0;
const RESET_FADE_MS: f32 = 5.0;
/// The longest stretch the splice search compares, in samples.
const MAX_WINDOW: i64 = 256;
/// The furthest the search moves a jump from the period, in samples.
const MAX_REACH: i64 = 64;

/// How the two heads are faded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Law {
    /// Raised cosine summing to one: for heads reading the same waveform.
    Gain,
    /// Sine/cosine summing to one in power: for unrelated material (a
    /// return to the minimum through noise or silence).
    Power,
}

/// A splice in progress: the outgoing head and how far through it is.
#[derive(Debug, Clone, Copy)]
struct Fade {
    /// The outgoing head's delay.
    from: f64,
    at: f32,
    len: f32,
    law: Law,
}

#[derive(Debug, Clone)]
pub struct Splicer {
    input: [Track; 2],
    /// Samples taken so far: the next input index.
    now: u64,
    /// The head's delay behind the newest sample.
    delay: f64,
    fade: Option<Fade>,
    /// Output pitch over input pitch, and the input's period (`None` when
    /// unvoiced), from the newest estimate.
    ratio: f32,
    period: Option<f32>,
    gain: f32,
    gain_target: f32,
    gain_step: f32,
    longest_period: f32,
    min_fade: f32,
    max_fade: f32,
    reset_fade: f32,
    reset_after: u32,
    unvoiced_for: u32,
    /// A head further back than this is brought back regardless.
    max_delay: f64,
    splices: u64,
}

impl Splicer {
    /// A live path at `sample_rate` that can carry periods up to
    /// `longest_period` samples.
    pub fn new(sample_rate: f32, longest_period: f32) -> Self {
        let longest = longest_period.ceil().max(16.0);
        let capacity = 4 * longest as usize + 4 * MAX_WINDOW as usize;
        let ms = |ms: f32| sample_rate * ms * 0.001;
        Self {
            input: [Track::new(capacity), Track::new(capacity)],
            now: 0,
            delay: MIN_DELAY,
            fade: None,
            ratio: 1.0,
            period: None,
            gain: 1.0,
            gain_target: 1.0,
            gain_step: 1.0 - (-1.0 / ms(GAIN_MS).max(1.0)).exp(),
            longest_period: longest,
            min_fade: ms(MIN_FADE_MS).max(8.0),
            max_fade: ms(MAX_FADE_MS).max(16.0),
            reset_fade: ms(RESET_FADE_MS).max(16.0),
            reset_after: ms(RESET_AFTER_MS).max(1.0) as u32,
            unvoiced_for: 0,
            max_delay: 2.5 * f64::from(longest) + MAX_WINDOW as f64,
            splices: 0,
        }
    }

    /// The fixed delay, for graph delay compensation.
    pub fn latency(&self) -> usize {
        LATENCY
    }

    /// The head's delay now, in samples: [`LATENCY`] plus what the
    /// shifting has wandered.
    pub fn delay(&self) -> f64 {
        self.delay
    }

    /// How many splices have been made.
    pub fn splices(&self) -> u64 {
        self.splices
    }

    /// The newest estimate's period (`None` when unvoiced), the pitch ratio
    /// to play it at and the level.
    pub fn set_control(&mut self, period: Option<f32>, ratio: f32, gain: f32) {
        self.period = period.map(|p| p.clamp(16.0, self.longest_period));
        self.ratio = if ratio.is_finite() {
            ratio.clamp(0.25, 4.0)
        } else {
            1.0
        };
        self.gain_target = if gain.is_finite() {
            gain.clamp(0.0, 2.0)
        } else {
            1.0
        };
    }

    /// Takes one input frame without playing: the path idles this way while
    /// the PSOLA path plays, so its history is whole when it comes back.
    #[inline]
    pub fn skip(&mut self, left: f32, right: f32) {
        let n = self.now as i64;
        *self.input[0].slot(n) = left;
        *self.input[1].slot(n) = right;
        self.now += 1;
    }

    /// Puts the head back at the minimum delay, as it starts: after
    /// [`Self::skip`], the output picks up from the input on time.
    pub fn restart(&mut self) {
        self.delay = MIN_DELAY;
        self.fade = None;
        self.gain = self.gain_target;
        self.unvoiced_for = 0;
    }

    pub fn reset(&mut self) {
        for track in self.input.iter_mut() {
            track.clear();
        }
        self.period = None;
        self.ratio = 1.0;
        self.gain_target = 1.0;
        self.restart();
    }

    /// Takes one input frame and returns the shifted frame and the plain
    /// input [`LATENCY`] samples ago.
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> ([f32; 2], [f32; 2]) {
        let n = self.now as i64;
        *self.input[0].slot(n) = left;
        *self.input[1].slot(n) = right;
        self.now += 1;

        if self.fade.is_none() {
            self.plan(n);
        }

        let here = n as f64 - self.delay;
        let mut wet = [self.input[0].read(here), self.input[1].read(here)];
        let speed = 1.0 - f64::from(self.ratio);
        if let Some(fade) = self.fade.as_mut() {
            let t = fade.at / fade.len;
            let (outgoing, incoming) = match fade.law {
                Law::Gain => {
                    let c = 0.5 + 0.5 * (std::f32::consts::PI * t).cos();
                    (c, 1.0 - c)
                }
                Law::Power => {
                    let angle = std::f32::consts::FRAC_PI_2 * t;
                    (angle.cos(), angle.sin())
                }
            };
            let there = n as f64 - fade.from;
            for (side, sample) in wet.iter_mut().enumerate() {
                *sample = *sample * incoming + self.input[side].read(there) * outgoing;
            }
            // Both heads run at the same speed. The outgoing one never
            // reads ahead of what the interpolator can reach.
            fade.from = (fade.from + speed).max(MIN_DELAY);
            fade.at += 1.0;
            if fade.at >= fade.len {
                self.fade = None;
            }
        }
        // Should a splice be held off (one already fading), the head waits
        // at the minimum rather than reading the future.
        self.delay = (self.delay + speed).max(MIN_DELAY);

        self.gain += (self.gain_target - self.gain) * self.gain_step;
        let dry = [
            self.input[0].at(n - LATENCY as i64),
            self.input[1].at(n - LATENCY as i64),
        ];
        ([wet[0] * self.gain, wet[1] * self.gain], dry)
    }

    /// Decides whether a splice starts at input sample `n`.
    fn plan(&mut self, n: i64) {
        let Some(period) = self.period else {
            // Unvoiced: once it lasts, bring the head back to the minimum
            // so the next phrase starts on time.
            self.unvoiced_for = self.unvoiced_for.saturating_add(1);
            if self.unvoiced_for >= self.reset_after && self.delay > MIN_DELAY + 0.5 {
                self.start(MIN_DELAY, self.reset_fade, Law::Power);
            }
            return;
        };
        self.unvoiced_for = 0;
        if self.delay > self.max_delay {
            self.start(MIN_DELAY, self.reset_fade, Law::Power);
            return;
        }
        let ratio = self.ratio;
        let len = self.fade_len(period, ratio);
        if ratio > 1.0 {
            // Catching up: jump a period back while the outgoing head still
            // has room to run out its fade.
            let room = f64::from((ratio - 1.0) * len) + 1.0;
            if self.delay <= MIN_DELAY + room {
                let jump = self.best_jump(n, period, true);
                self.start(self.delay + jump, len, Law::Gain);
            }
        } else if ratio < 1.0 {
            // Falling behind: jump a period forward once a whole one (and
            // the search's reach) is buffered ahead of the minimum.
            let reach = reach(period) as f64;
            if self.delay >= MIN_DELAY + f64::from(period) + reach {
                let jump = self.best_jump(n, period, false);
                self.start(self.delay - jump, len, Law::Gain);
            }
        }
    }

    fn start(&mut self, delay: f64, len: f32, law: Law) {
        self.fade = Some(Fade {
            from: self.delay,
            at: 0.0,
            len: len.max(1.0),
            law,
        });
        self.delay = delay.max(MIN_DELAY);
        self.splices += 1;
    }

    /// A fade a period long, but short enough to finish before the next
    /// splice is due — at a large ratio that comes round quickly.
    fn fade_len(&self, period: f32, ratio: f32) -> f32 {
        let drift = (ratio - 1.0).abs();
        let spacing = if drift > 1.0e-6 {
            0.5 * period / drift
        } else {
            f32::MAX
        };
        period
            .min(spacing)
            .clamp(self.min_fade, self.max_fade.max(self.min_fade))
    }

    /// The jump, in whole samples, that best lines the waveform before the
    /// new head up with the waveform before the head: the period, moved by
    /// up to [`reach`]. Backward for a head catching up, forward for one
    /// falling behind. Reads only buffered input.
    fn best_jump(&self, n: i64, period: f32, back: bool) -> f64 {
        let nominal = period.round() as i64;
        let reach = reach(period);
        let window = nominal.clamp(16, MAX_WINDOW);
        let head = n - self.delay.round() as i64;
        let mono = |i: i64| 0.5 * (self.input[0].at(i) + self.input[1].at(i));
        let lowest = (nominal - reach).max(1);
        let highest = if back {
            nominal + reach
        } else {
            // The new head must stay at or behind the minimum delay.
            (nominal + reach).min((self.delay - MIN_DELAY).floor() as i64)
        };
        let mut own = 0.0f32;
        for k in 0..window {
            let x = mono(head - k);
            own += x * x;
        }
        let mut best = (nominal.clamp(lowest, highest.max(lowest)), f32::MIN);
        for jump in lowest..=highest {
            let candidate = if back { head - jump } else { head + jump };
            let (mut cross, mut energy) = (0.0f32, 0.0f32);
            for k in 0..window {
                let y = mono(candidate - k);
                cross += mono(head - k) * y;
                energy += y * y;
            }
            let score = cross / (own * energy + 1.0e-12).sqrt();
            if score > best.1 {
                best = (jump, score);
            }
        }
        best.0 as f64
    }
}

/// How far the splice search moves a jump from the period.
fn reach(period: f32) -> i64 {
    ((period / 8.0).round() as i64).clamp(2, MAX_REACH)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    #[test]
    fn at_a_ratio_of_one_the_input_comes_out_two_samples_late() {
        let mut splicer = Splicer::new(SR, SR / 30.0);
        for n in 0..4_800 {
            let x = (n as f32 * 0.031).sin();
            if n == 1_000 {
                splicer.set_control(Some(240.0), 1.0, 1.0);
            }
            let (wet, dry) = splicer.process(x, -x);
            if n >= LATENCY {
                let expected = ((n - LATENCY) as f32 * 0.031).sin();
                assert!((wet[0] - expected).abs() < 1.0e-6, "{n}");
                assert!((wet[1] + expected).abs() < 1.0e-6, "{n}");
                assert_eq!(dry[0], wet[0]);
            }
        }
        assert_eq!(splicer.splices(), 0);
    }

    #[test]
    fn the_head_stays_within_about_a_period() {
        for ratio in [0.5f32, 0.94, 1.06, 2.0] {
            let period = 200.0;
            let mut splicer = Splicer::new(SR, SR / 30.0);
            splicer.set_control(Some(period), ratio, 1.0);
            let mut worst = 0.0f64;
            for n in 0..48_000 {
                let x = (std::f32::consts::TAU * n as f32 / period).sin();
                let _ = splicer.process(x, x);
                worst = worst.max(splicer.delay());
                assert!(splicer.delay() >= MIN_DELAY);
            }
            assert!(splicer.splices() > 10, "ratio {ratio}");
            // A period, the search's reach, and for a rise the fade's room.
            let room = f64::from(((ratio - 1.0).max(0.0) * period).min(0.5 * period)) + 1.0;
            let bound = MIN_DELAY + f64::from(period) + reach(period) as f64 + room;
            assert!(worst <= bound, "ratio {ratio}: {worst} > {bound}");
        }
    }
}

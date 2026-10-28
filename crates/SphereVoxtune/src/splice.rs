//! The live path: pitch shifting with no lookahead (pitch-synchronous splicer).
//! Borrowed from `Futureboard-release/crates/BuiltinAudioPlugins/crates/whitesharp/src/splice.rs`.

use crate::shifter::Track;

pub const LATENCY: usize = 2;

const MIN_DELAY: f64 = LATENCY as f64;
const GAIN_MS: f32 = 4.0;
const MIN_FADE_MS: f32 = 0.25;
const MAX_FADE_MS: f32 = 10.0;
const RESET_AFTER_MS: f32 = 12.0;
const RESET_FADE_MS: f32 = 5.0;
const MAX_WINDOW: i64 = 256;
const MAX_REACH: i64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Law {
    Gain,
    Power,
}

#[derive(Debug, Clone, Copy)]
struct Fade {
    from: f64,
    at: f32,
    len: f32,
    law: Law,
}

#[derive(Debug, Clone)]
pub struct Splicer {
    input: [Track; 2],
    now: u64,
    delay: f64,
    fade: Option<Fade>,
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
    max_delay: f64,
    splices: u64,
}

impl Splicer {
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

    pub fn latency(&self) -> usize {
        LATENCY
    }

    pub fn delay(&self) -> f64 {
        self.delay
    }

    pub fn splices(&self) -> u64 {
        self.splices
    }

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

    #[inline]
    pub fn skip(&mut self, left: f32, right: f32) {
        let n = self.now as i64;
        *self.input[0].slot(n) = left;
        *self.input[1].slot(n) = right;
        self.now += 1;
    }

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
            fade.from = (fade.from + speed).max(MIN_DELAY);
            fade.at += 1.0;
            if fade.at >= fade.len {
                self.fade = None;
            }
        }
        self.delay = (self.delay + speed).max(MIN_DELAY);

        self.gain += (self.gain_target - self.gain) * self.gain_step;
        let dry = [
            self.input[0].at(n - LATENCY as i64),
            self.input[1].at(n - LATENCY as i64),
        ];
        ([wet[0] * self.gain, wet[1] * self.gain], dry)
    }

    fn plan(&mut self, n: i64) {
        let Some(period) = self.period else {
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
            let room = f64::from((ratio - 1.0) * len) + 1.0;
            if self.delay <= MIN_DELAY + room {
                let jump = self.best_jump(n, period, true);
                self.start(self.delay + jump, len, Law::Gain);
            }
        } else if ratio < 1.0 {
            let r = reach(period) as f64;
            if self.delay >= MIN_DELAY + f64::from(period) + r {
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

    fn best_jump(&self, n: i64, period: f32, back: bool) -> f64 {
        let nominal = period.round() as i64;
        let r = reach(period);
        let window = nominal.clamp(16, MAX_WINDOW);
        let head = n - self.delay.round() as i64;
        let mono = |i: i64| 0.5 * (self.input[0].at(i) + self.input[1].at(i));
        let lowest = (nominal - r).max(1);
        let highest = if back {
            nominal + r
        } else {
            (nominal + r).min((self.delay - MIN_DELAY).floor() as i64)
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

pub(crate) fn reach(period: f32) -> i64 {
    ((period / 8.0).round() as i64).clamp(2, MAX_REACH)
}

//! Pitch-synchronous overlap-add (TD-PSOLA), stereo.
//!
//! Each output grain is two periods of the input, Hann-windowed, centred on
//! an analysis mark; marks sit one input period apart and grains are laid
//! one *output* period apart, so the pitch moves while each period keeps
//! its own shape — and with it the voice's formants. Reading a grain faster
//! or slower than it was recorded moves the formants instead (`formant`
//! below): that is what the throat control and formant correction steer.
//!
//! Unvoiced input goes through as plain overlap-add at a fixed grain, which
//! reconstructs it exactly. The whole path runs `latency` samples behind
//! the input: far enough that a grain never needs input that has not
//! arrived, or lands on output already read. Everything is sized at
//! construction; nothing allocates per sample.

/// One control frame: what to do with the input around `centre`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Control {
    /// Input time the frame was measured at, in samples.
    pub centre: u64,
    /// The input's period there, in samples; `None` when unvoiced.
    pub period: Option<f32>,
    /// Output pitch over input pitch.
    pub ratio: f32,
    /// How fast to read a grain: above 1 lifts the formants.
    pub formant: f32,
    /// The grain's level: the created vibrato's amplitude depth. Grains
    /// overlap, so a level changing grain to grain fades rather than steps.
    pub gain: f32,
}

const CONTROL_FRAMES: usize = 512;
/// The furthest a grain's read speed — its formant move — may go, either
/// way. It bounds how far a grain reaches, and so the delay.
pub const MAX_FORMANT: f32 = 1.5;

/// Power-of-two ring indexed by absolute sample number.
#[derive(Debug, Clone)]
struct Track {
    data: Box<[f32]>,
    mask: usize,
}

impl Track {
    fn new(capacity: usize) -> Self {
        let len = capacity.max(16).next_power_of_two();
        Self {
            data: vec![0.0; len].into_boxed_slice(),
            mask: len - 1,
        }
    }

    #[inline]
    fn at(&self, index: i64) -> f32 {
        self.data[(index as usize) & self.mask]
    }

    #[inline]
    fn slot(&mut self, index: i64) -> &mut f32 {
        &mut self.data[(index as usize) & self.mask]
    }

    /// Catmull-Rom read at a fractional position.
    #[inline]
    fn read(&self, position: f64) -> f32 {
        let whole = position.floor();
        let t = (position - whole) as f32;
        let i = whole as i64;
        let (p0, p1, p2, p3) = (self.at(i - 1), self.at(i), self.at(i + 1), self.at(i + 2));
        p1 + 0.5
            * t
            * (p2 - p0
                + t * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + t * (3.0 * (p1 - p2) + p3 - p0)))
    }

    fn clear(&mut self) {
        self.data.fill(0.0);
    }
}

#[derive(Debug, Clone)]
pub struct Shifter {
    input: [Track; 2],
    output: [Track; 2],
    weight: Track,
    controls: Box<[Control]>,
    control_head: usize,
    control_count: usize,
    /// Samples taken so far: the next input index.
    now: u64,
    latency: u64,
    /// How far ahead of the read point grains are laid.
    lookahead: f64,
    /// Grain half-length for unvoiced input.
    unvoiced_period: f32,
    longest_period: f32,
    next_grain: f64,
    mark: f64,
    /// The output sample being read now. A grain never writes behind it:
    /// that slot would not be read again until the ring came round, and
    /// then twice.
    read: i64,
}

impl Shifter {
    /// A shifter at `sample_rate` that can carry periods up to
    /// `longest_period` samples.
    pub fn new(sample_rate: f32, longest_period: f32) -> Self {
        let longest = longest_period.ceil() as usize;
        let capacity = 8 * longest + 64;
        let unvoiced_period = (sample_rate * 0.005).round().max(16.0);
        let mut shifter = Self {
            input: [Track::new(capacity), Track::new(capacity)],
            output: [Track::new(capacity), Track::new(capacity)],
            weight: Track::new(capacity),
            controls: vec![
                Control {
                    centre: 0,
                    period: None,
                    ratio: 1.0,
                    formant: 1.0,
                    gain: 1.0,
                };
                CONTROL_FRAMES
            ]
            .into_boxed_slice(),
            control_head: 0,
            control_count: 0,
            now: 0,
            latency: 0,
            lookahead: 0.0,
            unvoiced_period,
            longest_period,
            next_grain: 0.0,
            mark: 0.0,
            read: 0,
        };
        shifter.set_longest_period(longest_period, 0);
        shifter
    }

    /// The delay the shifter runs at for periods up to `longest_period`
    /// (at most the one it was built for), plus `extra` samples of
    /// analysis lag the controls arrive with.
    pub fn latency_for(longest_period: f32, unvoiced_period: f32, extra: usize) -> u64 {
        let p = longest_period.max(unvoiced_period) as f64;
        // Grains are laid up to 1.5p ahead of the read point (a grain read
        // at the slowest speed spans ±1.5p) and reach 1.5p further into the
        // input.
        (3.0 * p).ceil() as u64 + extra as u64 + 4
    }

    /// Retunes the delay for an input type reaching `longest_period`. Only
    /// between notes: the output restarts from the delayed input.
    pub fn set_longest_period(&mut self, longest_period: f32, extra: usize) {
        let p = longest_period
            .min(self.longest_period)
            .max(self.unvoiced_period);
        self.latency = Self::latency_for(p, self.unvoiced_period, extra);
        self.lookahead = f64::from(MAX_FORMANT) * p as f64;
        self.next_grain = self.now as f64 - self.latency as f64;
        self.mark = self.next_grain;
        for track in self.output.iter_mut() {
            track.clear();
        }
        self.weight.clear();
    }

    pub fn latency(&self) -> u64 {
        self.latency
    }

    pub fn unvoiced_period(&self) -> f32 {
        self.unvoiced_period
    }

    pub fn reset(&mut self) {
        for track in self.input.iter_mut().chain(self.output.iter_mut()) {
            track.clear();
        }
        self.weight.clear();
        self.control_count = 0;
        self.next_grain = self.now as f64 - self.latency as f64;
        self.mark = self.next_grain;
    }

    /// Records a control frame. Frames arrive in time order.
    pub fn push_control(&mut self, control: Control) {
        self.control_head = (self.control_head + 1) % CONTROL_FRAMES;
        self.controls[self.control_head] = control;
        self.control_count = (self.control_count + 1).min(CONTROL_FRAMES);
    }

    /// The newest frame measured at or before `at`.
    fn control_at(&self, at: f64) -> Control {
        let mut index = self.control_head;
        let mut fallback = None;
        for _ in 0..self.control_count {
            let control = self.controls[index];
            if control.centre as f64 <= at {
                return control;
            }
            fallback = Some(control);
            index = (index + CONTROL_FRAMES - 1) % CONTROL_FRAMES;
        }
        fallback.unwrap_or(Control {
            centre: 0,
            period: None,
            ratio: 1.0,
            formant: 1.0,
            gain: 1.0,
        })
    }

    /// Takes one input frame and returns the shifted frame from
    /// [`Self::latency`] samples ago, and the plain input from then.
    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> ([f32; 2], [f32; 2]) {
        let n = self.now as i64;
        *self.input[0].slot(n) = left;
        *self.input[1].slot(n) = right;
        self.now += 1;

        let read = n - self.latency as i64;
        self.read = read;
        while self.next_grain <= read as f64 + self.lookahead {
            self.lay_grain();
        }

        // Where grains overlap more than the window's own overlap — the
        // pitch raised — scale back by the square root: a voice's pulses
        // keep their energy, and a ratio of one is untouched.
        let weight = self.weight.at(read).max(1.0).sqrt();
        let mut wet = [0.0; 2];
        for (side, track) in self.output.iter_mut().enumerate() {
            let slot = track.slot(read);
            wet[side] = *slot / weight;
            *slot = 0.0;
        }
        *self.weight.slot(read) = 0.0;
        let dry = [self.input[0].at(read), self.input[1].at(read)];
        (wet, dry)
    }

    fn lay_grain(&mut self) {
        let centre = self.next_grain;
        let control = self.control_at(centre);
        let (period, ratio, formant) = match control.period {
            Some(period) => (
                period.clamp(16.0, self.longest_period),
                control.ratio.clamp(0.25, 4.0),
                control.formant.clamp(1.0 / MAX_FORMANT, MAX_FORMANT),
            ),
            None => (self.unvoiced_period, 1.0, 1.0),
        };
        let gain = control.gain.clamp(0.0, 2.0);
        let period = period as f64;

        // The analysis mark nearest the grain. Marks step a whole period at
        // a time — after each grain, and here to catch up or fall back — so
        // successive grains are successive (or repeated) periods of the
        // input. At a ratio of one the marks and the grains step together
        // and the input comes back exactly, whatever the period does.
        if (self.mark - centre).abs() > period * 1.5 {
            self.mark = centre;
        }
        while self.mark + period * 0.5 < centre {
            self.mark += period;
        }
        while self.mark - period * 0.5 > centre {
            self.mark -= period;
        }

        let formant = formant as f64;
        let half = period / formant;
        let first = ((centre - half).ceil() as i64).max(self.read);
        let last = (centre + half).floor() as i64;
        for k in first..=last {
            let offset = k as f64 - centre;
            let w = (0.5 + 0.5 * (std::f64::consts::PI * offset / half).cos()) as f32;
            if w <= 0.0 {
                continue;
            }
            let source = self.mark + offset * formant;
            for side in 0..2 {
                let sample = self.input[side].read(source);
                *self.output[side].slot(k) += w * sample * gain;
            }
            *self.weight.slot(k) += w;
        }
        self.next_grain += period / ratio as f64;
        self.mark += period;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(hz: f32, ratio: f32, voiced: bool) -> (Vec<f32>, u64) {
        let period = SR / hz;
        let mut shifter = Shifter::new(SR, SR / 60.0);
        let lag = 0;
        shifter.set_longest_period(SR / 60.0, lag);
        let latency = shifter.latency();
        let mut out = Vec::new();
        for n in 0..(SR as usize) {
            if n % 192 == 0 {
                shifter.push_control(Control {
                    centre: n as u64,
                    period: voiced.then_some(period),
                    ratio,
                    formant: 1.0,
                    gain: 1.0,
                });
            }
            let x = tone(hz, n);
            let (wet, _) = shifter.process(x, x);
            out.push(wet[0]);
        }
        (out, latency)
    }

    /// A voice-like tone: a fundamental and falling harmonics.
    fn tone(hz: f32, n: usize) -> f32 {
        let t = n as f32 / SR;
        (1..5)
            .map(|k| (std::f32::consts::TAU * hz * k as f32 * t).sin() / k as f32)
            .sum::<f32>()
            * 0.3
    }

    /// The pitch of the steady part, by the crate's own detector.
    fn frequency(signal: &[f32]) -> f32 {
        let mut detector = crate::detect::Detector::new(SR, 30.0);
        detector.set_range(50.0, 1_500.0);
        let mut periods = Vec::new();
        for (n, x) in signal.iter().enumerate() {
            if let Some(estimate) = detector.push(*x, 0.2) {
                if n > signal.len() / 2 {
                    periods.extend(estimate.period);
                }
            }
        }
        periods.sort_by(f32::total_cmp);
        SR / periods[periods.len() / 2]
    }

    #[test]
    fn unity_passes_the_input_through_delayed() {
        for voiced in [true, false] {
            let (out, latency) = run(220.0, 1.0, voiced);
            let latency = latency as usize;
            let mut worst = 0.0f32;
            for (n, y) in out.iter().enumerate().skip(latency + 4_800) {
                let x = tone(220.0, n - latency);
                worst = worst.max((y - x).abs());
            }
            assert!(worst < 0.01, "voiced {voiced}: off by {worst}");
        }
    }

    #[test]
    fn a_ratio_moves_the_pitch_by_it() {
        for ratio in [0.75f32, 1.0595, 1.25, 1.5] {
            let (out, _) = run(220.0, ratio, true);
            let found = frequency(&out);
            let cents = 1_200.0 * (found / (220.0 * ratio)).log2();
            assert!(
                cents.abs() < 15.0,
                "ratio {ratio}: {found} Hz ({cents:+.0} c)"
            );
            let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
            let input: Vec<f32> = (0..out.len() / 2).map(|n| tone(220.0, n)).collect();
            let level = 20.0 * (rms(&out[out.len() / 2..]) / rms(&input)).log10();
            assert!(level.abs() < 4.0, "ratio {ratio}: level {level:+.1} dB");
        }
    }
}

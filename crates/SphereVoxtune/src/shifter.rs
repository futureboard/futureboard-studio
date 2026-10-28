//! Pitch-synchronous overlap-add (TD-PSOLA), stereo.
//! Borrowed from `Futureboard-release/crates/BuiltinAudioPlugins/crates/whitesharp/src/shifter.rs`.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Control {
    pub centre: u64,
    pub period: Option<f32>,
    pub ratio: f32,
    pub formant: f32,
    pub gain: f32,
}

const CONTROL_FRAMES: usize = 512;
pub const MAX_FORMANT: f32 = 1.5;

#[derive(Debug, Clone)]
pub(crate) struct Track {
    data: Box<[f32]>,
    mask: usize,
}

impl Track {
    pub(crate) fn new(capacity: usize) -> Self {
        let len = capacity.max(16).next_power_of_two();
        Self {
            data: vec![0.0; len].into_boxed_slice(),
            mask: len - 1,
        }
    }

    #[inline]
    pub(crate) fn at(&self, index: i64) -> f32 {
        self.data[(index as usize) & self.mask]
    }

    #[inline]
    pub(crate) fn slot(&mut self, index: i64) -> &mut f32 {
        &mut self.data[(index as usize) & self.mask]
    }

    /// Catmull-Rom read at a fractional position.
    #[inline]
    pub(crate) fn read(&self, position: f64) -> f32 {
        let whole = position.floor();
        let t = (position - whole) as f32;
        let i = whole as i64;
        let (p0, p1, p2, p3) = (self.at(i - 1), self.at(i), self.at(i + 1), self.at(i + 2));
        p1 + 0.5
            * t
            * (p2 - p0
                + t * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + t * (3.0 * (p1 - p2) + p3 - p0)))
    }

    pub(crate) fn clear(&mut self) {
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
    now: u64,
    latency: u64,
    lookahead: f64,
    unvoiced_period: f32,
    longest_period: f32,
    next_grain: f64,
    mark: f64,
    read: i64,
}

impl Shifter {
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

    pub fn latency_for(longest_period: f32, unvoiced_period: f32, extra: usize) -> u64 {
        let p = longest_period.max(unvoiced_period) as f64;
        (3.0 * p).ceil() as u64 + extra as u64 + 4
    }

    pub fn latency_for_input(
        sample_rate: f32,
        built_for: f32,
        longest_period: f32,
        extra: usize,
    ) -> u64 {
        let unvoiced = (sample_rate * 0.005).round().max(16.0);
        let p = longest_period.min(built_for).max(unvoiced);
        Self::latency_for(p, unvoiced, extra)
    }

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

    pub fn now(&self) -> u64 {
        self.now
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

    #[inline]
    pub fn skip(&mut self, left: f32, right: f32) {
        let n = self.now as i64;
        *self.input[0].slot(n) = left;
        *self.input[1].slot(n) = right;
        self.now += 1;
    }

    pub fn resume(&mut self) {
        for track in self.output.iter_mut() {
            track.clear();
        }
        self.weight.clear();
        self.next_grain = self.now as f64 - self.latency as f64;
        self.mark = self.next_grain;
    }

    pub fn push_control(&mut self, control: Control) {
        self.control_head = (self.control_head + 1) % CONTROL_FRAMES;
        self.controls[self.control_head] = control;
        self.control_count = (self.control_count + 1).min(CONTROL_FRAMES);
    }

    pub(crate) fn control_at(&self, at: f64) -> Control {
        let mut index = self.control_head;
        let mut later: Option<Control> = None;
        for _ in 0..self.control_count {
            let control = self.controls[index];
            if control.centre as f64 <= at {
                return match later {
                    Some(next) => Self::between(control, next, at),
                    None => control,
                };
            }
            later = Some(control);
            index = (index + CONTROL_FRAMES - 1) % CONTROL_FRAMES;
        }
        later.unwrap_or(Control {
            centre: 0,
            period: None,
            ratio: 1.0,
            formant: 1.0,
            gain: 1.0,
        })
    }

    fn between(earlier: Control, later: Control, at: f64) -> Control {
        let (Some(from), Some(to)) = (earlier.period, later.period) else {
            return earlier;
        };
        let span = later.centre.saturating_sub(earlier.centre) as f64;
        if span <= 0.0 {
            return earlier;
        }
        let t = ((at - earlier.centre as f64) / span).clamp(0.0, 1.0) as f32;
        let lerp = |a: f32, b: f32| a + (b - a) * t;
        Control {
            centre: earlier.centre,
            period: Some(lerp(from, to)),
            ratio: lerp(earlier.ratio, later.ratio),
            formant: lerp(earlier.formant, later.formant),
            gain: lerp(earlier.gain, later.gain),
        }
    }

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

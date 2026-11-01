//! Pitch-keeping time stretch for notes whose length was edited.
//!
//! Grains one source pitch period apart are laid one period apart in the
//! output, so a note spread over a longer span (or squeezed into a shorter
//! one) keeps its pitch and formants: a period is repeated or skipped where
//! needed (TD-PSOLA). The source is read straight from memory, so the stretch
//! adds no delay, and the shifter after it tunes the note as usual.

use crate::shifter::Track;

/// Stretches one note at a time; moving to another note, or jumping in time
/// (seek, loop), starts it afresh.
#[derive(Debug, Clone)]
pub struct Stretcher {
    output: Track,
    weight: Track,
    longest_period: f64,
    /// Note being stretched, as the caller keys it.
    key: Option<usize>,
    /// Next output sample to hand out.
    pos: i64,
    /// Output position of the next grain's centre.
    next_grain: f64,
    /// Source position of the last grain's centre.
    mark: f64,
}

impl Stretcher {
    pub fn new(longest_period: f32) -> Self {
        let capacity = 8 * longest_period.ceil().max(16.0) as usize + 64;
        Self {
            output: Track::new(capacity),
            weight: Track::new(capacity),
            longest_period: f64::from(longest_period.max(16.0)),
            key: None,
            pos: 0,
            next_grain: 0.0,
            mark: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.key = None;
    }

    /// Output sample `at` of note `key`. `grain(t)` gives, for output sample
    /// position `t`, the matching source position and the source pitch
    /// period there (both in samples).
    pub fn sample(
        &mut self,
        key: usize,
        at: i64,
        source: &[f32],
        grain: impl Fn(f64) -> (f64, f64),
    ) -> f32 {
        if self.key != Some(key) || self.pos != at {
            self.output.clear();
            self.weight.clear();
            self.key = Some(key);
            self.pos = at;
            self.next_grain = at as f64;
            self.mark = grain(at as f64).0;
        }
        // Every grain reaching back to `at` must be laid before it is read.
        loop {
            let (target, period) = grain(self.next_grain);
            let period = period.clamp(16.0, self.longest_period);
            if self.next_grain - period > at as f64 {
                break;
            }
            self.lay_grain(source, target, period);
        }
        let weight = std::mem::take(self.weight.slot(at));
        let sum = std::mem::take(self.output.slot(at));
        self.pos = at + 1;
        if weight > 1e-6 { sum / weight } else { 0.0 }
    }

    fn lay_grain(&mut self, source: &[f32], target: f64, period: f64) {
        let centre = self.next_grain;
        // `mark` is where the source naturally continues. Stay there unless
        // it drifted half a period from where the source should be; then
        // repeat or skip a period, unless it is far off (a jump), then go
        // straight there.
        let natural = self.mark;
        if (natural - target).abs() > period * 1.5 {
            self.mark = target;
        } else {
            let mut mark = natural;
            while mark + period * 0.5 < target {
                mark += period;
            }
            while mark - period * 0.5 > target {
                mark -= period;
            }
            if mark != natural {
                // The period is only an estimate: land where the waveform
                // matches what the output is already playing, or the join
                // catches audibly.
                mark = align(source, natural, mark, period);
            }
            self.mark = mark;
        }
        let first = ((centre - period).ceil() as i64).max(self.pos);
        let last = (centre + period).floor() as i64;
        for k in first..=last {
            let offset = k as f64 - centre;
            let w = (0.5 + 0.5 * (std::f64::consts::PI * offset / period).cos()) as f32;
            if w <= 0.0 {
                continue;
            }
            *self.output.slot(k) += w * read(source, self.mark + offset);
            *self.weight.slot(k) += w;
        }
        self.next_grain += period;
        self.mark += period;
    }
}

/// The position within a quarter period of `guess` whose surroundings best
/// match the source around `natural` (normalised cross-correlation over a
/// period either side, every other sample).
fn align(source: &[f32], natural: f64, guess: f64, period: f64) -> f64 {
    let reach = (period / 4.0) as i64;
    let half = period as i64;
    let mut best = guess;
    let mut best_score = f32::NEG_INFINITY;
    for shift in -reach..=reach {
        let at = guess + shift as f64;
        let (mut dot, mut energy) = (0.0f32, 0.0f32);
        for k in (-half..half).step_by(2) {
            let a = read(source, at + k as f64);
            dot += a * read(source, natural + k as f64);
            energy += a * a;
        }
        let score = dot / energy.max(1e-9).sqrt();
        if score > best_score {
            best_score = score;
            best = at;
        }
    }
    best
}

/// Linear read at a fractional sample position; silence outside the source.
#[inline]
fn read(source: &[f32], position: f64) -> f32 {
    if position < 0.0 {
        return 0.0;
    }
    let i = position.floor() as usize;
    let Some(&a) = source.get(i) else {
        return 0.0;
    };
    let b = source.get(i + 1).copied().unwrap_or(a);
    a + (b - a) * (position - i as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Detector;

    fn median_hz(signal: &[f32], sr: f32) -> f32 {
        let mut detector = Detector::new(sr, 30.0);
        detector.set_range(50.0, 1_500.0);
        let mut periods = Vec::new();
        for (n, &s) in signal.iter().enumerate() {
            if let Some(est) = detector.push(s, 0.2)
                && n > signal.len() / 4
            {
                periods.extend(est.period);
            }
        }
        periods.sort_by(f32::total_cmp);
        sr / periods[periods.len() / 2]
    }

    /// A sung-like vowel: vibrato, slow drift, cycle-to-cycle jitter and two
    /// formants. Returns the samples and the smooth f0 (Hz) per sample.
    fn voice(sr: f32, secs: f32) -> (Vec<f32>, Vec<f32>) {
        let n = (sr * secs) as usize;
        let mut seed = 0x2545_f491u32;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32 - 0.5
        };
        let mut phase = 0.0f32;
        let mut jitter = 1.0f32;
        let mut f0s = Vec::with_capacity(n);
        let mut raw = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / sr;
            let cents = 45.0 * (std::f32::consts::TAU * 5.3 * t).sin() + 30.0 * t;
            let f0 = 196.0 * 2f32.powf(cents / 1200.0);
            f0s.push(f0);
            phase += f0 * jitter / sr;
            if phase >= 1.0 {
                phase -= 1.0;
                jitter = 1.0 + 0.008 * rand();
            }
            let s: f32 = (1..40)
                .map(|k| (std::f32::consts::TAU * k as f32 * phase).sin() / (k as f32).powf(1.1))
                .sum();
            raw.push(s * 0.12);
        }
        // Two resonators (formants near 650 and 1100 Hz).
        let mut out = raw.clone();
        for (fc, bw) in [(650.0f32, 90.0f32), (1_100.0, 110.0)] {
            let r = (-std::f32::consts::PI * bw / sr).exp();
            let c = 2.0 * r * (std::f32::consts::TAU * fc / sr).cos();
            let (mut y1, mut y2) = (0.0f32, 0.0f32);
            for x in out.iter_mut() {
                let y = *x * (1.0 - r) + c * y1 - r * r * y2;
                y2 = y1;
                y1 = y;
                *x = y + *x * 0.3;
            }
        }
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        out.iter_mut().for_each(|s| *s *= 0.5 / peak);
        (out, f0s)
    }

    /// How far each 10 ms frame is from repeating itself one (true) period
    /// later: high at a bad join. Returns (median, 98th percentile).
    fn roughness(x: &[f32], period_at: impl Fn(usize) -> f32, sr: f32) -> (f32, f32) {
        let frame = (sr * 0.01) as usize;
        let mut scores = Vec::new();
        let mut i = frame * 4;
        while i + frame < x.len() - frame * 4 {
            let (mut e, mut p) = (0.0f32, 0.0f32);
            for j in i..i + frame {
                let back = j as f32 - period_at(j);
                let k = back.floor() as usize;
                let f = back - k as f32;
                let prev = x[k] + (x[k + 1] - x[k]) * f;
                e += (x[j] - prev).powi(2);
                p += x[j].powi(2);
            }
            scores.push((e / p.max(1e-9)).sqrt());
            i += frame;
        }
        scores.sort_by(f32::total_cmp);
        (scores[scores.len() / 2], scores[scores.len() * 98 / 100])
    }

    #[test]
    fn a_stretched_voice_joins_as_smoothly_as_it_was_sung() {
        let sr = 48_000.0f32;
        let (src, f0s) = voice(sr, 1.2);
        // The engine's period: 32 averaged contour points, a little off.
        let points: Vec<f32> = (0..32)
            .map(|k| {
                let a = k * f0s.len() / 32;
                let b = ((k + 1) * f0s.len() / 32).min(f0s.len());
                f0s[a..b].iter().sum::<f32>() / (b - a) as f32 * 1.003
            })
            .collect();
        let est = |pos: f64| {
            let u = (pos / f0s.len() as f64).clamp(0.0, 1.0) * 31.0;
            let i = (u.floor() as usize).min(30);
            let hz = points[i] + (points[i + 1] - points[i]) * (u - i as f64) as f32;
            f64::from(sr / hz)
        };
        let base = roughness(&src, |j| sr / f0s[j], sr);
        for stretch in [1.3f64, 1.7, 0.75] {
            let mut st = Stretcher::new(sr / 50.0);
            let len = (src.len() as f64 * stretch) as i64;
            let out: Vec<f32> = (0..len)
                .map(|at| {
                    st.sample(0, at, &src, |t| {
                        let p = t / stretch;
                        (p, est(p))
                    })
                })
                .collect();
            let r = roughness(
                &out,
                |j| {
                    let s = ((j as f64 / stretch) as usize).min(f0s.len() - 1);
                    sr / f0s[s]
                },
                sr,
            );
            // Repeated or skipped periods must not stand out as catches.
            assert!(
                r.1 < base.1 * 1.4,
                "×{stretch}: p98 {:.3} vs sung {:.3}",
                r.1,
                base.1
            );
        }
    }

    #[test]
    fn stretching_keeps_pitch_and_level() {
        let sr = 48_000.0f32;
        let hz = 220.0f32;
        let source: Vec<f32> = (0..sr as usize / 2)
            .map(|n| {
                let t = n as f32 / sr;
                (1..5)
                    .map(|k| (std::f32::consts::TAU * hz * k as f32 * t).sin() / k as f32)
                    .sum::<f32>()
                    * 0.3
            })
            .collect();
        let period = f64::from(sr / hz);
        for stretch in [2.0f64, 0.6] {
            let mut st = Stretcher::new(sr / 50.0);
            let len = (source.len() as f64 * stretch) as i64;
            let out: Vec<f32> = (0..len)
                .map(|at| st.sample(0, at, &source, |t| (t / stretch, period)))
                .collect();
            let found = median_hz(&out, sr);
            let cents = 1_200.0 * (found / hz).log2();
            assert!(cents.abs() < 10.0, "×{stretch}: {found} Hz ({cents} c)");
            let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
            let mid = &out[out.len() / 4..out.len() * 3 / 4];
            let ratio = rms(mid) / rms(&source);
            assert!((0.85..1.15).contains(&ratio), "×{stretch}: level {ratio}");
        }
    }
}

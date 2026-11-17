//! Band-limited sample-rate conversion: a Kaiser-windowed sinc evaluated from a phase table
//! (interpolated between phases), any ratio, streaming.

use crate::AudioBuffer;

/// Zero crossings on each side of the kernel centre.
const HALF: usize = 16;
/// Phases per input sample in the kernel table.
const PHASES: usize = 512;
const KAISER_BETA: f64 = 8.6;

/// A streaming resampler for a fixed channel count and rate pair.
#[derive(Clone, Debug)]
pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    /// Input samples per output sample.
    step: f64,
    /// Kernel half width in input samples (wider when downsampling).
    width: usize,
    /// `(PHASES + 1) × 2·width` kernel values.
    table: Vec<f32>,
    /// Per-channel input history; `buf[c][0]` is input sample `base`.
    buf: Vec<Vec<f32>>,
    base: i64,
    /// Position of the next output sample in input samples.
    t: f64,
    inputs: u64,
    outputs: u64,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        assert!(in_rate > 0 && out_rate > 0);
        let step = f64::from(in_rate) / f64::from(out_rate);
        // cutoff relative to the input Nyquist: lower it when downsampling
        let cutoff = if step > 1.0 { 0.97 / step } else { 0.97 };
        let width = ((HALF as f64 / cutoff.min(1.0)).ceil() as usize).max(HALF);
        let taps = 2 * width;
        let mut table = vec![0.0f32; (PHASES + 1) * taps];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            for i in 0..taps {
                let d = i as f64 - (width as f64 - 1.0) - frac;
                table[p * taps + i] = kernel(d, cutoff, width as f64) as f32;
            }
        }
        Self {
            in_rate,
            out_rate,
            channels,
            step,
            width,
            table,
            buf: vec![vec![0.0; width]; channels],
            base: -(width as i64),
            t: 0.0,
            inputs: 0,
            outputs: 0,
        }
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }

    /// Feed planar input; returns the output samples that are complete.
    pub fn process(&mut self, input: &[Vec<f32>]) -> AudioBuffer {
        let n = input.first().map_or(0, Vec::len);
        for (c, b) in self.buf.iter_mut().enumerate() {
            match input.get(c) {
                Some(src) => b.extend_from_slice(&src[..n.min(src.len())]),
                None => b.extend(std::iter::repeat_n(0.0, n)),
            }
        }
        self.inputs += n as u64;
        self.run(None)
    }

    /// Output the rest (the input followed by silence), so that the total output length is
    /// `ceil(inputs × out_rate / in_rate)`.
    pub fn flush(&mut self) -> AudioBuffer {
        let total = (self.inputs as u128 * u128::from(self.out_rate))
            .div_ceil(u128::from(self.in_rate)) as u64;
        for b in &mut self.buf {
            b.extend(std::iter::repeat_n(0.0, self.width + 1));
        }
        self.run(Some(total))
    }

    /// Forget all state.
    pub fn reset(&mut self) {
        *self = Resampler::new(self.in_rate, self.out_rate, self.channels);
    }

    fn run(&mut self, limit: Option<u64>) -> AudioBuffer {
        let taps = 2 * self.width;
        let avail_end = self.base + self.buf.first().map_or(0, Vec::len) as i64;
        let mut out = vec![Vec::new(); self.channels];
        loop {
            if limit.is_some_and(|l| self.outputs >= l) {
                break;
            }
            let center = self.t.floor() as i64;
            let first = center - self.width as i64 + 1;
            if first + taps as i64 > avail_end {
                break;
            }
            let pf = (self.t - center as f64) * PHASES as f64;
            let p = (pf as usize).min(PHASES - 1);
            let a = (pf - p as f64) as f32;
            let k0 = &self.table[p * taps..(p + 1) * taps];
            let k1 = &self.table[(p + 1) * taps..(p + 2) * taps];
            let off = (first - self.base) as usize;
            for (c, o) in out.iter_mut().enumerate() {
                let x = &self.buf[c][off..off + taps];
                let mut s0 = 0.0f32;
                let mut s1 = 0.0f32;
                for i in 0..taps {
                    s0 += x[i] * k0[i];
                    s1 += x[i] * k1[i];
                }
                o.push(s0 + (s1 - s0) * a);
            }
            self.outputs += 1;
            self.t = self.outputs as f64 * self.step;
        }
        // drop history no longer needed
        let keep_from = self.t.floor() as i64 - self.width as i64 + 1;
        if keep_from > self.base {
            let d = ((keep_from - self.base) as usize).min(self.buf.first().map_or(0, Vec::len));
            for b in &mut self.buf {
                b.drain(..d);
            }
            self.base += d as i64;
        }
        AudioBuffer {
            sample_rate: self.out_rate,
            channels: out,
        }
    }
}

fn kernel(d: f64, cutoff: f64, width: f64) -> f64 {
    let x = d / width;
    if x.abs() >= 1.0 {
        return 0.0;
    }
    let s = if d.abs() < 1e-12 {
        1.0
    } else {
        let a = std::f64::consts::PI * cutoff * d;
        a.sin() / a
    };
    cutoff * s * bessel_i0(KAISER_BETA * (1.0 - x * x).sqrt()) / bessel_i0(KAISER_BETA)
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Resample a whole buffer to `rate` (a copy when the rate already matches).
pub fn resample(buf: &AudioBuffer, rate: u32) -> AudioBuffer {
    if buf.sample_rate == rate || buf.sample_rate == 0 {
        let mut b = buf.clone();
        b.sample_rate = rate;
        return b;
    }
    let mut r = Resampler::new(buf.sample_rate, rate, buf.channels.len());
    let mut out = r.process(&buf.channels);
    out.append(&r.flush());
    out.sample_rate = rate;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_survives_44k_to_48k() {
        let f = 1000.0;
        let n = 44100;
        let x: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / 44100.0).sin())
            .collect();
        let buf = AudioBuffer {
            sample_rate: 44100,
            channels: vec![x],
        };
        let out = resample(&buf, 48000);
        assert_eq!(out.frames(), 48000);
        for i in 1000..47000 {
            let want = (2.0 * std::f64::consts::PI * f as f64 * i as f64 / 48000.0).sin() as f32;
            assert!(
                (out.channels[0][i] - want).abs() < 2e-3,
                "sample {i}: {} vs {want}",
                out.channels[0][i]
            );
        }
    }

    #[test]
    fn streaming_equals_one_shot() {
        let x: Vec<f32> = (0..5000)
            .map(|i| ((i * 13 % 97) as f32 / 48.0) - 1.0)
            .collect();
        let one = resample(
            &AudioBuffer {
                sample_rate: 48000,
                channels: vec![x.clone()],
            },
            22050,
        );
        let mut r = Resampler::new(48000, 22050, 1);
        let mut s = AudioBuffer::default();
        for chunk in x.chunks(333) {
            s.append(&r.process(&[chunk.to_vec()]));
        }
        s.append(&r.flush());
        assert_eq!(one.frames(), s.frames());
        for (a, b) in one.channels[0].iter().zip(&s.channels[0]) {
            assert!((a - b).abs() < 1e-6);
        }
    }
}

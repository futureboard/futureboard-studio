//! Zero-latency partitioned convolution: up to two inputs, two outputs.
//!
//! The first [`BLOCK`] taps of each response run as a plain FIR on every
//! sample; the rest run as a uniformly partitioned overlap-save convolution
//! whose output for a block is computed, from inputs that are already in, as
//! the block before it completes. So nothing is late: the FIR covers the
//! block the FFT work cannot yet see.
//!
//! Realtime: the responses ([`PartitionedIr`]) are built off the audio
//! thread and shared; the streaming state ([`StreamingConvolver`]) is
//! allocated in `new` and never again. The FFT work lands once per block.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Partition length, samples.
pub(crate) const BLOCK: usize = 128;
const FFT_LEN: usize = 2 * BLOCK;
/// Non-redundant bins of a real signal's spectrum.
const BINS: usize = BLOCK + 1;

/// The FFTs every partitioned response and convolver of this size share.
#[derive(Clone)]
pub(crate) struct Ffts {
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

impl std::fmt::Debug for Ffts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ffts")
    }
}

impl Ffts {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        Self {
            forward: planner.plan_fft_forward(FFT_LEN),
            inverse: planner.plan_fft_inverse(FFT_LEN),
        }
    }

    fn scratch_len(&self) -> usize {
        self.forward
            .get_inplace_scratch_len()
            .max(self.inverse.get_inplace_scratch_len())
    }
}

/// Impulse responses from up to two inputs to two outputs, cut into a
/// time-domain head and frequency-domain partitions.
#[derive(Debug, Clone)]
pub(crate) struct PartitionedIr {
    inputs: usize,
    /// `[input][output]`: the first `BLOCK` taps, time-reversed.
    head: [[Vec<f32>; 2]; 2],
    /// `[input][output]`: partitions `1..`, `BINS` bins each.
    parts: [[Vec<Complex32>; 2]; 2],
    partitions: usize,
}

impl PartitionedIr {
    /// `irs[input][output]`, for one or two inputs. Allocates; control
    /// thread only.
    pub fn new(irs: &[[Vec<f32>; 2]], ffts: &Ffts) -> Self {
        let inputs = irs.len().clamp(1, 2);
        let longest = irs
            .iter()
            .flat_map(|pair| pair.iter())
            .map(Vec::len)
            .max()
            .unwrap_or(0);
        let partitions = longest.saturating_sub(BLOCK).div_ceil(BLOCK);
        let mut scratch = vec![Complex32::default(); ffts.scratch_len()];
        let mut buffer = vec![Complex32::default(); FFT_LEN];
        let mut head: [[Vec<f32>; 2]; 2] = Default::default();
        let mut parts: [[Vec<Complex32>; 2]; 2] = Default::default();
        for (input, pair) in irs.iter().take(2).enumerate() {
            for (output, ir) in pair.iter().enumerate() {
                let tap = |i: usize| ir.get(i).copied().unwrap_or(0.0);
                head[input][output] = (0..BLOCK).map(|k| tap(BLOCK - 1 - k)).collect();
                let spectra = &mut parts[input][output];
                spectra.reserve(partitions * BINS);
                for p in 1..=partitions {
                    for (i, slot) in buffer.iter_mut().enumerate() {
                        *slot =
                            Complex32::new(if i < BLOCK { tap(p * BLOCK + i) } else { 0.0 }, 0.0);
                    }
                    ffts.forward.process_with_scratch(&mut buffer, &mut scratch);
                    spectra.extend_from_slice(&buffer[..BINS]);
                }
            }
        }
        Self {
            inputs,
            head,
            parts,
            partitions,
        }
    }

    pub fn partitions(&self) -> usize {
        self.partitions
    }
}

/// The streaming side: histories, the frequency-domain delay line, and the
/// output the FFT half has ready for the block now playing.
#[derive(Debug, Clone)]
pub(crate) struct StreamingConvolver {
    ffts: Ffts,
    /// Per input: the last `BLOCK` samples, written twice so the head FIR
    /// reads one contiguous slice.
    history: [Vec<f32>; 2],
    history_pos: usize,
    previous: [[f32; BLOCK]; 2],
    current: [[f32; BLOCK]; 2],
    /// Per input: the spectra of the last `capacity` blocks.
    delay_line: [Vec<Complex32>; 2],
    capacity: usize,
    newest: usize,
    ready: [[f32; BLOCK]; 2],
    pos: usize,
    buffer: Vec<Complex32>,
    sum: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

impl StreamingConvolver {
    /// State for responses of up to `max_partitions` partitions. Allocates.
    pub fn new(ffts: Ffts, max_partitions: usize) -> Self {
        let capacity = max_partitions.max(1);
        let scratch = vec![Complex32::default(); ffts.scratch_len()];
        Self {
            ffts,
            history: [vec![0.0; 2 * BLOCK], vec![0.0; 2 * BLOCK]],
            history_pos: 0,
            previous: [[0.0; BLOCK]; 2],
            current: [[0.0; BLOCK]; 2],
            delay_line: [
                vec![Complex32::default(); capacity * BINS],
                vec![Complex32::default(); capacity * BINS],
            ],
            capacity,
            newest: 0,
            ready: [[0.0; BLOCK]; 2],
            pos: 0,
            buffer: vec![Complex32::default(); FFT_LEN],
            sum: vec![Complex32::default(); BINS],
            scratch,
        }
    }

    /// Forget everything heard: for a new response, after a fade to silence.
    pub fn reset(&mut self) {
        for h in &mut self.history {
            h.fill(0.0);
        }
        for d in &mut self.delay_line {
            d.fill(Complex32::default());
        }
        self.previous = [[0.0; BLOCK]; 2];
        self.current = [[0.0; BLOCK]; 2];
        self.ready = [[0.0; BLOCK]; 2];
        self.history_pos = 0;
        self.newest = 0;
        self.pos = 0;
    }

    /// One sample in per input, one sample out per output.
    #[inline]
    pub fn tick(&mut self, input: [f32; 2], ir: &PartitionedIr) -> [f32; 2] {
        let inputs = ir.inputs;
        self.history_pos += 1;
        if self.history_pos == BLOCK {
            self.history_pos = 0;
        }
        for s in 0..inputs {
            self.history[s][self.history_pos] = input[s];
            self.history[s][self.history_pos + BLOCK] = input[s];
            self.current[s][self.pos] = input[s];
        }
        let mut out = [self.ready[0][self.pos], self.ready[1][self.pos]];
        for s in 0..inputs {
            let window = &self.history[s][self.history_pos + 1..self.history_pos + 1 + BLOCK];
            for (o, value) in out.iter_mut().enumerate() {
                *value += dot(window, &ir.head[s][o]);
            }
        }
        self.pos += 1;
        if self.pos == BLOCK {
            self.pos = 0;
            self.complete_block(ir);
        }
        out
    }

    /// A block of input is in: take its spectrum, and work out the next
    /// block's output from every partition past the first.
    fn complete_block(&mut self, ir: &PartitionedIr) {
        let inputs = ir.inputs;
        self.newest = (self.newest + 1) % self.capacity;
        for s in 0..inputs {
            for i in 0..BLOCK {
                self.buffer[i] = Complex32::new(self.previous[s][i], 0.0);
                self.buffer[BLOCK + i] = Complex32::new(self.current[s][i], 0.0);
            }
            self.ffts
                .forward
                .process_with_scratch(&mut self.buffer, &mut self.scratch);
            let at = self.newest * BINS;
            self.delay_line[s][at..at + BINS].copy_from_slice(&self.buffer[..BINS]);
            self.previous[s] = self.current[s];
        }
        let partitions = ir.partitions.min(self.capacity);
        for o in 0..2 {
            self.sum.fill(Complex32::default());
            for s in 0..inputs {
                let spectra = &ir.parts[s][o];
                for k in 1..=partitions {
                    let slot = (self.newest + self.capacity - (k - 1)) % self.capacity;
                    let x = &self.delay_line[s][slot * BINS..slot * BINS + BINS];
                    let h = &spectra[(k - 1) * BINS..k * BINS];
                    for b in 0..BINS {
                        self.sum[b] += x[b] * h[b];
                    }
                }
            }
            // Back to a full, conjugate-symmetric spectrum.
            self.buffer[..BINS].copy_from_slice(&self.sum);
            for b in 1..BLOCK {
                self.buffer[FFT_LEN - b] = self.sum[b].conj();
            }
            self.ffts
                .inverse
                .process_with_scratch(&mut self.buffer, &mut self.scratch);
            let scale = 1.0 / FFT_LEN as f32;
            for i in 0..BLOCK {
                self.ready[o][i] = self.buffer[BLOCK + i].re * scale;
            }
        }
    }
}

/// `Σ a·b`, in eight lanes so it vectorises.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut lanes = [0.0f32; 8];
    let mut ca = a.chunks_exact(8);
    let mut cb = b.chunks_exact(8);
    for (x, y) in (&mut ca).zip(&mut cb) {
        for i in 0..8 {
            lanes[i] += x[i] * y[i];
        }
    }
    let mut sum = lanes.iter().sum::<f32>();
    for (x, y) in ca.remainder().iter().zip(cb.remainder()) {
        sum += x * y;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct(x: &[f32], h: &[f32]) -> Vec<f32> {
        (0..x.len())
            .map(|n| {
                (0..h.len())
                    .filter(|&k| k <= n)
                    .map(|k| h[k] * x[n - k])
                    .sum()
            })
            .collect()
    }

    #[test]
    fn matches_direct_convolution_with_no_latency() {
        let ffts = Ffts::new();
        let mut seed = 7u32;
        let mut noise = |len: usize| -> Vec<f32> {
            (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
                })
                .collect()
        };
        let irs = [[noise(700), noise(333)], [noise(129), noise(1000)]];
        let x = [noise(3000), noise(3000)];
        let ir = PartitionedIr::new(&irs, &ffts);
        let mut conv = StreamingConvolver::new(ffts, ir.partitions());
        let mut out = [Vec::new(), Vec::new()];
        for n in 0..3000 {
            let y = conv.tick([x[0][n], x[1][n]], &ir);
            out[0].push(y[0]);
            out[1].push(y[1]);
        }
        for o in 0..2 {
            let a = direct(&x[0], &irs[0][o]);
            let b = direct(&x[1], &irs[1][o]);
            for n in 0..3000 {
                let want = a[n] + b[n];
                assert!(
                    (out[o][n] - want).abs() < 1.0e-3,
                    "out {o} at {n}: {} vs {want}",
                    out[o][n]
                );
            }
        }
    }
}

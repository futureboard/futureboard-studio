//! Output loudness metering: ITU-R BS.1770-4 K-weighting, momentary
//! (400 ms), short-term (3 s), gated integrated loudness, EBU Tech 3342
//! loudness range, and a held true-peak maximum.
//!
//! Realtime shape: fixed-size state only. Energy is summed per 100 ms hop
//! into a 30-slot ring; at each hop the momentary and short-term windows are
//! read off the ring, the 400 ms gating block's energy goes into a fixed
//! histogram (0.1 LU bins from −70 to +5 LUFS, each keeping its count and
//! energy sum), and integrated loudness and LRA are read off the
//! histograms. A hop does two bounded passes over 750 bins, ten times a
//! second — no allocation, no unbounded work.

/// Hops in the short-term window: 3 s of 100 ms.
const SHORT_HOPS: usize = 30;
/// Hops in a momentary / gating block: 400 ms.
const MOMENTARY_HOPS: usize = 4;
const HIST_MIN_LUFS: f32 = -70.0;
const HIST_STEP_LU: f32 = 0.1;
/// −70 … +5 LUFS.
const HIST_BINS: usize = 750;
/// BS.1770's absolute gate.
const ABSOLUTE_GATE_LUFS: f32 = -70.0;
const RELATIVE_GATE_LU: f32 = -10.0;
/// EBU Tech 3342's relative gate for the loudness range.
const LRA_RELATIVE_GATE_LU: f32 = -20.0;
/// What a reading with nothing measured yet reads.
pub const NO_READING: f32 = -120.0;

/// BS.1770 loudness of a mean-square energy summed over channels.
#[inline]
fn lufs(energy: f64) -> f32 {
    if energy <= 1.0e-20 {
        NO_READING
    } else {
        (-0.691 + 10.0 * energy.log10()) as f32
    }
}

#[inline]
fn energy_of(lufs: f32) -> f64 {
    10f64.powf((f64::from(lufs) + 0.691) / 10.0)
}

fn bin_of(lufs: f32) -> usize {
    (((lufs - HIST_MIN_LUFS) / HIST_STEP_LU).floor().max(0.0) as usize).min(HIST_BINS - 1)
}

fn bin_center(bin: usize) -> f32 {
    HIST_MIN_LUFS + (bin as f32 + 0.5) * HIST_STEP_LU
}

/// One second-order section in direct form I, in `f64` (the K-weighting
/// high-pass sits at 38 Hz, where `f32` state loses precision at 192 kHz).
#[derive(Debug, Clone, Copy, Default)]
struct Section {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Section {
    #[inline]
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1]
            - self.a[0] * self.y[0]
            - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        // Flush what has decayed to nothing, so silence does not go denormal.
        let y = if y.abs() < 1.0e-30 { 0.0 } else { y };
        self.y = [y, self.y[0]];
        y
    }

    fn clear(&mut self) {
        self.x = [0.0; 2];
        self.y = [0.0; 2];
    }
}

/// BS.1770's two K-weighting stages for `sample_rate`: the head's high
/// shelf, then the RLB high-pass. The analogue prototypes are the ones the
/// standard's 48 kHz coefficients come from, so other rates get the same
/// curve.
fn k_weighting(sample_rate: f32) -> [Section; 2] {
    let fs = f64::from(sample_rate.max(1.0));
    let shelf = {
        let f0 = 1_681.974_450_955_533;
        let gain_db = 3.999_843_853_973_347;
        let q = 0.707_175_236_955_419_6;
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let vh = 10f64.powf(gain_db / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / q + k * k;
        Section {
            b: [
                (vh + vb * k / q + k * k) / a0,
                2.0 * (k * k - vh) / a0,
                (vh - vb * k / q + k * k) / a0,
            ],
            a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
            ..Section::default()
        }
    };
    let high_pass = {
        let f0 = 38.135_470_876_024_44;
        let q = 0.500_327_037_323_877_3;
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let a0 = 1.0 + k / q + k * k;
        Section {
            b: [1.0, -2.0, 1.0],
            a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
            ..Section::default()
        }
    };
    [shelf, high_pass]
}

/// The output's loudness readings.
#[derive(Debug, Clone)]
pub struct Loudness {
    filters: [[Section; 2]; 2],
    hop_len: usize,
    hop_pos: usize,
    hop_acc: f64,
    hops: [f64; SHORT_HOPS],
    hop_write: usize,
    hop_count: usize,
    pub momentary: f32,
    pub short_term: f32,
    pub integrated: f32,
    pub range: f32,
    /// Gating blocks above the absolute gate since the last reset.
    pub gated_blocks: u64,
    block_count: [u32; HIST_BINS],
    block_energy: [f64; HIST_BINS],
    short_count: [u32; HIST_BINS],
    short_energy: [f64; HIST_BINS],
    short_values: u64,
}

impl Loudness {
    pub fn new(sample_rate: f32) -> Self {
        let filters = k_weighting(sample_rate);
        Self {
            filters: [filters, filters],
            hop_len: ((sample_rate.max(1.0) * 0.1).round() as usize).max(1),
            hop_pos: 0,
            hop_acc: 0.0,
            hops: [0.0; SHORT_HOPS],
            hop_write: 0,
            hop_count: 0,
            momentary: NO_READING,
            short_term: NO_READING,
            integrated: NO_READING,
            range: 0.0,
            gated_blocks: 0,
            block_count: [0; HIST_BINS],
            block_energy: [0.0; HIST_BINS],
            short_count: [0; HIST_BINS],
            short_energy: [0.0; HIST_BINS],
            short_values: 0,
        }
    }

    /// Everything back to nothing measured.
    pub fn reset(&mut self) {
        for channel in &mut self.filters {
            for section in channel {
                section.clear();
            }
        }
        self.hop_pos = 0;
        self.hop_acc = 0.0;
        self.hops = [0.0; SHORT_HOPS];
        self.hop_write = 0;
        self.hop_count = 0;
        self.momentary = NO_READING;
        self.short_term = NO_READING;
        self.reset_program();
    }

    /// The program-long readings — integrated loudness and the range — back
    /// to nothing measured; the momentary and short-term windows run on.
    pub fn reset_program(&mut self) {
        self.integrated = NO_READING;
        self.range = 0.0;
        self.gated_blocks = 0;
        self.block_count.fill(0);
        self.block_energy.fill(0.0);
        self.short_count.fill(0);
        self.short_energy.fill(0.0);
        self.short_values = 0;
    }

    #[inline]
    pub fn push(&mut self, left: f32, right: f32) {
        let [[shelf_l, pass_l], [shelf_r, pass_r]] = &mut self.filters;
        let yl = pass_l.run(shelf_l.run(f64::from(left)));
        let yr = pass_r.run(shelf_r.run(f64::from(right)));
        self.hop_acc += yl * yl + yr * yr;
        self.hop_pos += 1;
        if self.hop_pos >= self.hop_len {
            self.finish_hop();
        }
    }

    /// Mean-square energy over the newest `hops` hops.
    fn window_energy(&self, hops: usize) -> f64 {
        let n = hops.min(self.hop_count).max(1);
        let sum: f64 = (0..n)
            .map(|age| self.hops[(self.hop_write + SHORT_HOPS - 1 - age) % SHORT_HOPS])
            .sum();
        sum / (n * self.hop_len) as f64
    }

    fn finish_hop(&mut self) {
        self.hops[self.hop_write] = self.hop_acc;
        self.hop_write = (self.hop_write + 1) % SHORT_HOPS;
        self.hop_count = self.hop_count.saturating_add(1);
        self.hop_acc = 0.0;
        self.hop_pos = 0;

        let block = self.window_energy(MOMENTARY_HOPS);
        self.momentary = lufs(block);
        self.short_term = lufs(self.window_energy(SHORT_HOPS));

        // A complete 400 ms block, 75 % overlapped: a gating block.
        if self.hop_count >= MOMENTARY_HOPS && self.momentary > ABSOLUTE_GATE_LUFS {
            let bin = bin_of(self.momentary);
            self.block_count[bin] += 1;
            self.block_energy[bin] += block;
            self.gated_blocks += 1;
            self.integrated = self.gated_integrated();
        }
        // A complete short-term window: a loudness-range value.
        if self.hop_count >= SHORT_HOPS && self.short_term > ABSOLUTE_GATE_LUFS {
            let bin = bin_of(self.short_term);
            self.short_count[bin] += 1;
            self.short_energy[bin] += energy_of(self.short_term);
            self.short_values += 1;
            self.range = self.loudness_range();
        }
    }

    /// BS.1770 integrated loudness: the mean of the blocks over the
    /// absolute gate, then again of those within 10 LU of that mean.
    fn gated_integrated(&self) -> f32 {
        let (count, energy) = self
            .block_count
            .iter()
            .zip(&self.block_energy)
            .fold((0u64, 0.0f64), |(c, e), (&n, &sum)| {
                (c + u64::from(n), e + sum)
            });
        if count == 0 {
            return NO_READING;
        }
        let gate = lufs(energy / count as f64) + RELATIVE_GATE_LU;
        let (count, energy) = (0..HIST_BINS).filter(|&bin| bin_center(bin) > gate).fold(
            (0u64, 0.0f64),
            |(c, e), bin| {
                (
                    c + u64::from(self.block_count[bin]),
                    e + self.block_energy[bin],
                )
            },
        );
        if count == 0 {
            NO_READING
        } else {
            lufs(energy / count as f64)
        }
    }

    /// EBU Tech 3342 loudness range: the short-term values within 20 LU of
    /// their mean, 10th to 95th percentile.
    fn loudness_range(&self) -> f32 {
        let (count, energy) = self
            .short_count
            .iter()
            .zip(&self.short_energy)
            .fold((0u64, 0.0f64), |(c, e), (&n, &sum)| {
                (c + u64::from(n), e + sum)
            });
        if count == 0 {
            return 0.0;
        }
        let gate = lufs(energy / count as f64) + LRA_RELATIVE_GATE_LU;
        let first = (0..HIST_BINS)
            .find(|&bin| bin_center(bin) > gate)
            .unwrap_or(HIST_BINS);
        let kept: u64 = self.short_count[first..]
            .iter()
            .map(|&n| u64::from(n))
            .sum();
        if kept == 0 {
            return 0.0;
        }
        let percentile = |fraction: f64| {
            let rank = ((kept as f64 - 1.0) * fraction).round() as u64;
            let mut seen = 0u64;
            for bin in first..HIST_BINS {
                seen += u64::from(self.short_count[bin]);
                if seen > rank {
                    return bin_center(bin);
                }
            }
            bin_center(HIST_BINS - 1)
        };
        (percentile(0.95) - percentile(0.10)).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_sine(meter: &mut Loudness, amplitude: f32, seconds: f32) {
        let rate = 48_000.0;
        for n in 0..(seconds * rate) as usize {
            let x = (std::f32::consts::TAU * 1_000.0 * n as f32 / rate).sin() * amplitude;
            meter.push(x, x);
        }
    }

    /// BS.1770's reference: a 1 kHz sine at 0 dBFS peak in one channel reads
    /// −3.01 LUFS, so −20 dBFS in both reads −20.0.
    #[test]
    fn a_stereo_1k_sine_at_minus_20_reads_minus_20_lufs() {
        let mut meter = Loudness::new(48_000.0);
        run_sine(&mut meter, 0.1, 5.0);
        for reading in [meter.momentary, meter.short_term, meter.integrated] {
            assert!((reading + 20.0).abs() < 0.3, "{reading}");
        }
        assert!(meter.range < 0.5, "steady tone LRA {}", meter.range);
    }

    #[test]
    fn integrated_gates_out_silence_and_range_sees_level_steps() {
        let mut meter = Loudness::new(48_000.0);
        run_sine(&mut meter, 0.1, 6.0);
        run_sine(&mut meter, 0.0, 6.0);
        assert!(
            (meter.integrated + 20.0).abs() < 0.3,
            "{}",
            meter.integrated
        );

        // A 10 LU step with no silence between: the range is the step.
        let mut stepped = Loudness::new(48_000.0);
        run_sine(&mut stepped, 0.1, 10.0);
        run_sine(&mut stepped, 0.1 * 10f32.powf(-10.0 / 20.0), 10.0);
        assert!(
            stepped.range > 9.0 && stepped.range < 11.0,
            "{}",
            stepped.range
        );
        meter.reset_program();
        assert_eq!(meter.integrated, NO_READING);
        assert_eq!(meter.gated_blocks, 0);
    }
}

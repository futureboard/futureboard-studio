//! Four-way Linkwitz–Riley band splitter shared by the multi-band built-ins.
//!
//! 4th-order Linkwitz–Riley splits, with the all-pass each branch needs to
//! line its phase up with the other, so the four bands sum back to the input
//! through one shared all-pass — no comb, no level change. Imager splits mid
//! and side through it; the Compressor splits left and right.
//!
//! Realtime contract: every filter is allocated in [`FourBandSplitter::new`];
//! `run` and `retune` only do arithmetic on them.

use biquad::{Biquad, Coefficients, DirectForm1, Q_BUTTERWORTH_F32, ToHertz, Type};

/// Bands out of one [`FourBandSplitter`].
pub const SPLIT_BANDS: usize = 4;
/// Crossover frequencies a [`FourBandSplitter`] is tuned by, low to high.
pub const SPLIT_CROSSOVERS: usize = SPLIT_BANDS - 1;

fn coefficients(kind: Type<f32>, hz: f32, sample_rate: f32) -> Coefficients<f32> {
    // Every caller has already clamped `hz` into the filter's range, so the
    // only failure left is a malformed rate — which the callers rule out. A
    // flat pass-through is still the right answer if it ever happens.
    Coefficients::<f32>::from_params(kind, sample_rate.hz(), hz.hz(), Q_BUTTERWORTH_F32).unwrap_or(
        Coefficients {
            a1: 0.0,
            a2: 0.0,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
        },
    )
}

/// Two identical Butterworth sections in series: one side of a 4th-order
/// Linkwitz–Riley crossover. Its low and high outputs sum to a 2nd-order
/// all-pass at the same frequency, which is what the compensation below leans
/// on.
#[derive(Debug, Clone)]
struct Lr4 {
    a: DirectForm1<f32>,
    b: DirectForm1<f32>,
}

impl Lr4 {
    fn new(coefficients: Coefficients<f32>) -> Self {
        Self {
            a: DirectForm1::<f32>::new(coefficients),
            b: DirectForm1::<f32>::new(coefficients),
        }
    }

    /// Retune in place; the running state is kept so a moved crossover does
    /// not reset the signal through it.
    fn retune(&mut self, coefficients: Coefficients<f32>) {
        self.a.update_coefficients(coefficients);
        self.b.update_coefficients(coefficients);
    }

    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        self.b.run(self.a.run(x))
    }

    fn reset(&mut self) {
        self.a.reset_state();
        self.b.reset_state();
    }
}

/// One Linkwitz–Riley split into a low and a high output.
#[derive(Debug, Clone)]
struct Split {
    low: Lr4,
    high: Lr4,
}

impl Split {
    fn new(hz: f32, sample_rate: f32) -> Self {
        Self {
            low: Lr4::new(coefficients(Type::LowPass, hz, sample_rate)),
            high: Lr4::new(coefficients(Type::HighPass, hz, sample_rate)),
        }
    }

    fn retune(&mut self, hz: f32, sample_rate: f32) {
        self.low
            .retune(coefficients(Type::LowPass, hz, sample_rate));
        self.high
            .retune(coefficients(Type::HighPass, hz, sample_rate));
    }

    #[inline]
    fn run(&mut self, x: f32) -> (f32, f32) {
        (self.low.run(x), self.high.run(x))
    }

    fn reset(&mut self) {
        self.low.reset();
        self.high.reset();
    }
}

/// Four bands out of one signal.
///
/// The middle crossover splits first; each half is split again at its own
/// crossover. A half only carries the phase of the split it went through, so
/// each is also passed through the all-pass of the split it did *not* — the
/// low half through the top crossover's, the high half through the bottom's.
/// Every band then carries the same all-pass product, and the four sum back to
/// the input through it.
///
/// `hz` must be sorted low to high and already held inside what a biquad can
/// be tuned to at the sample rate.
#[derive(Debug, Clone)]
pub struct FourBandSplitter {
    middle: Split,
    bottom: Split,
    top: Split,
    low_align: DirectForm1<f32>,
    high_align: DirectForm1<f32>,
}

impl FourBandSplitter {
    pub fn new(hz: [f32; SPLIT_CROSSOVERS], sample_rate: f32) -> Self {
        Self {
            bottom: Split::new(hz[0], sample_rate),
            middle: Split::new(hz[1], sample_rate),
            top: Split::new(hz[2], sample_rate),
            low_align: DirectForm1::<f32>::new(coefficients(Type::AllPass, hz[2], sample_rate)),
            high_align: DirectForm1::<f32>::new(coefficients(Type::AllPass, hz[0], sample_rate)),
        }
    }

    pub fn retune(&mut self, hz: [f32; SPLIT_CROSSOVERS], sample_rate: f32) {
        self.bottom.retune(hz[0], sample_rate);
        self.middle.retune(hz[1], sample_rate);
        self.top.retune(hz[2], sample_rate);
        self.low_align
            .update_coefficients(coefficients(Type::AllPass, hz[2], sample_rate));
        self.high_align
            .update_coefficients(coefficients(Type::AllPass, hz[0], sample_rate));
    }

    #[inline]
    pub fn run(&mut self, x: f32) -> [f32; SPLIT_BANDS] {
        let (low, high) = self.middle.run(x);
        let low = self.low_align.run(low);
        let high = self.high_align.run(high);
        let (b0, b1) = self.bottom.run(low);
        let (b2, b3) = self.top.run(high);
        [b0, b1, b2, b3]
    }

    pub fn reset(&mut self) {
        self.middle.reset();
        self.bottom.reset();
        self.top.reset();
        self.low_align.reset_state();
        self.high_align.reset_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bands of a sine sum back to its level at every frequency, and the
    /// band the sine sits in carries almost all of it.
    #[test]
    fn bands_sum_flat_and_separate() {
        let rate = 48_000.0f32;
        for (hz, home) in [(50.0f32, 0), (400.0, 1), (2_500.0, 2), (12_000.0, 3)] {
            let mut splitter = FourBandSplitter::new([120.0, 1_000.0, 6_000.0], rate);
            let warm = 12_000;
            let window = 24_000;
            let mut sum_energy = 0.0f64;
            let mut band_energy = [0.0f64; SPLIT_BANDS];
            for i in 0..warm + window {
                let x = (std::f32::consts::TAU * hz * i as f32 / rate).sin();
                let bands = splitter.run(x);
                if i >= warm {
                    let sum: f32 = bands.iter().sum();
                    sum_energy += f64::from(sum * sum);
                    for (energy, band) in band_energy.iter_mut().zip(bands) {
                        *energy += f64::from(band * band);
                    }
                }
            }
            let rms = (sum_energy / window as f64).sqrt();
            assert!(
                (rms - 0.5f64.sqrt()).abs() < 0.01,
                "{hz} Hz summed to {rms}"
            );
            let total: f64 = band_energy.iter().sum();
            assert!(
                band_energy[home] / total > 0.8,
                "{hz} Hz did not land in band {home}: {band_energy:?}"
            );
        }
    }
}

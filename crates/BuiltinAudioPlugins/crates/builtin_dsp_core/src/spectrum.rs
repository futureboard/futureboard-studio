//! Realtime spectrum analysis for a built-in insert's audio.
//!
//! One [`SpectrumAnalyzer`] lives per built-in insert, wherever the insert
//! runs: Studio's plug-in host process, or LiveStage's engine. It is fed the
//! block arriving at the insert. It is deliberately split in two halves:
//!
//! * [`SpectrumAnalyzer::push_block`] runs on every block and does nothing but
//!   copy samples into a preallocated ring — no FFT, no allocation, no branchy
//!   work proportional to anything but the frame count.
//! * [`SpectrumAnalyzer::analyze`] runs at most [`FRAMES_PER_ANALYSIS`] apart
//!   (~30 Hz at typical rates) and does the window + FFT + binning. The plan and
//!   every scratch buffer are allocated once in [`SpectrumAnalyzer::new`], so
//!   this is also allocation-free in steady state.
//!
//! The result is a purely visual signal: it drives an editor's analyser
//! overlay and nothing else reads it. That is what makes a torn read across a
//! frame boundary acceptable where it is published (the plug-in host's shared
//! region, LiveStage's insert telemetry) — no decision, automation value, or
//! audio path depends on these numbers.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Bins published per frame, spaced logarithmically across [`MIN_HZ`]..[`MAX_HZ`]
/// so the editor's log frequency axis maps to them one-to-one.
pub const SPECTRUM_BINS: usize = 128;

/// Analysis window. 4096 points is ~12 Hz resolution at 48 kHz. The larger
/// window is worth it at the bottom of the range, where an EQ is adjusted in
/// steps far finer than a 2048-point transform can separate; at one transform
/// per [`FRAMES_PER_ANALYSIS`] it is still a rounding error in the producer
/// thread's block budget.
pub const FFT_SIZE: usize = 4096;

/// Lowest and highest frequency the published bins span. Matches the editor's
/// axis range; anything outside is not represented rather than folded in.
pub const MIN_HZ: f32 = 20.0;
pub const MAX_HZ: f32 = 20_000.0;

/// Quietest level a bin reports. Bins at or below this read as "no energy".
pub const FLOOR_DB: f32 = -100.0;
/// Loudest level a bin reports, so the published range is a fixed, documented
/// scale both sides agree on rather than an open-ended float.
pub const CEIL_DB: f32 = 0.0;

/// Samples between analyses. At 48 kHz this is ~31 Hz, matching the editor's
/// telemetry tick; running the FFT faster than the UI consumes it is waste.
pub const FRAMES_PER_ANALYSIS: usize = 1536;

/// Per-frame smoothing. Rises fast so transients read honestly, falls slowly so
/// the display stays legible — the standard analyser ballistics.
const ATTACK: f32 = 0.55;
const RELEASE: f32 = 0.12;

/// A log-spaced magnitude spectrum of one insert's signal.
///
/// Not `Clone`: the FFT scratch is sized for one in-flight analysis and the
/// type is owned by exactly one thread.
pub struct SpectrumAnalyzer {
    sample_rate: f32,
    fft: Arc<dyn Fft<f32>>,
    /// Rolling capture of the most recent [`FFT_SIZE`] mono samples.
    ring: Box<[f32; FFT_SIZE]>,
    write: usize,
    /// Samples captured since the last analysis, for the throttle.
    since_analysis: usize,
    /// Precomputed Hann window, so the hot half never calls `cos`.
    window: Box<[f32; FFT_SIZE]>,
    /// Windowed, de-ringed input to the transform (reused every analysis).
    work: Box<[Complex<f32>; FFT_SIZE]>,
    scratch: Vec<Complex<f32>>,
    /// First FFT bin (inclusive) feeding each output bin; `edges[i + 1]` is the
    /// exclusive end. Precomputed from the sample rate.
    edges: Box<[usize; SPECTRUM_BINS + 1]>,
    /// Smoothed output in dB, in `FLOOR_DB..=CEIL_DB`.
    levels: Box<[f32; SPECTRUM_BINS]>,
}

impl SpectrumAnalyzer {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        let fft = FftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];

        let mut window = Box::new([0.0f32; FFT_SIZE]);
        for (i, w) in window.iter_mut().enumerate() {
            let phase = std::f32::consts::TAU * i as f32 / FFT_SIZE as f32;
            *w = 0.5 - 0.5 * phase.cos();
        }

        let mut analyzer = Self {
            sample_rate,
            fft,
            ring: Box::new([0.0; FFT_SIZE]),
            write: 0,
            since_analysis: 0,
            window,
            work: Box::new([Complex::new(0.0, 0.0); FFT_SIZE]),
            scratch,
            edges: Box::new([0; SPECTRUM_BINS + 1]),
            levels: Box::new([FLOOR_DB; SPECTRUM_BINS]),
        };
        analyzer.rebuild_edges();
        analyzer
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        let sample_rate = sample_rate.max(1.0);
        if (self.sample_rate - sample_rate).abs() < f32::EPSILON {
            return;
        }
        self.sample_rate = sample_rate;
        self.rebuild_edges();
        self.reset();
    }

    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.write = 0;
        self.since_analysis = 0;
        self.levels.fill(FLOOR_DB);
    }

    /// Map each output bin onto a half-open range of FFT bins, straight from
    /// the log frequency map — no widening.
    ///
    /// At the bottom of the range the log steps are finer than the transform
    /// can resolve, so consecutive output bins legitimately land on the *same*
    /// FFT bin. `analyze` reads at least one bin regardless, which makes the
    /// low end a visible stair. That stair is the analyser's real resolution;
    /// nudging each edge forward to keep the ranges disjoint would hide it by
    /// quietly turning the bottom two octaves into a linear map, putting a
    /// 100 Hz tone a fifth of the display away from 100 Hz.
    fn rebuild_edges(&mut self) {
        let hz_per_bin = self.sample_rate / FFT_SIZE as f32;
        let nyquist_bin = FFT_SIZE / 2;
        let log_min = MIN_HZ.ln();
        let log_span = (MAX_HZ / MIN_HZ).ln();
        for i in 0..=SPECTRUM_BINS {
            let hz = (log_min + log_span * i as f32 / SPECTRUM_BINS as f32).exp();
            let bin = (hz / hz_per_bin).round() as usize;
            self.edges[i] = bin.clamp(1, nyquist_bin);
        }
    }

    /// Capture one block. Cheap by design: a mono sum into the rolling window,
    /// nothing else. Safe to call from the audio producer thread.
    pub fn push_block(&mut self, left: &[f32], right: &[f32]) {
        let frames = left.len().min(right.len());
        for i in 0..frames {
            self.ring[self.write] = 0.5 * (left[i] + right[i]);
            self.write = (self.write + 1) % FFT_SIZE;
        }
        self.since_analysis = self.since_analysis.saturating_add(frames);
    }

    /// Whether enough new audio has arrived to justify another transform.
    pub fn analysis_due(&self) -> bool {
        self.since_analysis >= FRAMES_PER_ANALYSIS
    }

    /// Window, transform and bin the captured audio into smoothed dB levels.
    /// Returns `None` when no analysis was due, so the caller can skip
    /// publishing rather than republish a frame it already sent.
    pub fn analyze(&mut self) -> Option<&[f32; SPECTRUM_BINS]> {
        if !self.analysis_due() {
            return None;
        }
        self.since_analysis = 0;

        // Unroll the ring oldest-first so the window is applied to a
        // time-ordered signal, not across the wrap seam.
        for i in 0..FFT_SIZE {
            let sample = self.ring[(self.write + i) % FFT_SIZE];
            self.work[i] = Complex::new(sample * self.window[i], 0.0);
        }
        self.fft
            .process_with_scratch(self.work.as_mut_slice(), &mut self.scratch);

        // Coherent gain of a Hann window is 0.5, and a real signal splits its
        // energy across the mirrored halves — normalise so a full-scale sine
        // reads 0 dB rather than an arbitrary window-dependent number.
        let norm = 4.0 / FFT_SIZE as f32;
        for i in 0..SPECTRUM_BINS {
            let start = self.edges[i];
            // `.max(start + 1)` is what makes a zero-width range still read a
            // bin: below the transform's resolution neighbouring output bins
            // share one, rather than reporting an empty range as silence.
            let end = self.edges[i + 1].max(start + 1).min(FFT_SIZE / 2 + 1);
            let mut peak = 0.0f32;
            for bin in start..end {
                peak = peak.max(self.work[bin].norm_sqr());
            }
            let db = if peak > 0.0 {
                // norm_sqr is amplitude²: 10·log10 of it is the same as
                // 20·log10 of the amplitude, without the extra sqrt.
                (10.0 * (peak * norm * norm).log10()).clamp(FLOOR_DB, CEIL_DB)
            } else {
                FLOOR_DB
            };
            let previous = self.levels[i];
            let coeff = if db > previous { ATTACK } else { RELEASE };
            self.levels[i] = previous + (db - previous) * coeff;
        }
        Some(&self.levels)
    }

    /// The most recent levels without running an analysis.
    pub fn levels(&self) -> &[f32; SPECTRUM_BINS] {
        &self.levels
    }
}

/// Compress one bin to the byte the editor receives: `0` is [`FLOOR_DB`], `255`
/// is [`CEIL_DB`]. A ~0.4 dB step is finer than the display can resolve, and it
/// keeps a 128-bin frame at 128 bytes instead of a kilobyte of JSON floats.
pub fn quantize_db(db: f32) -> u8 {
    let t = (db - FLOOR_DB) / (CEIL_DB - FLOOR_DB);
    (t.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `hz` at full scale until an analysis is due.
    fn analyze_tone(
        analyzer: &mut SpectrumAnalyzer,
        sample_rate: f32,
        hz: f32,
    ) -> [f32; SPECTRUM_BINS] {
        let mut phase = 0.0f32;
        let step = std::f32::consts::TAU * hz / sample_rate;
        let mut left = [0.0f32; 256];
        let mut right = [0.0f32; 256];
        // Several passes so the smoother settles near the real level.
        for _ in 0..64 {
            for i in 0..256 {
                let s = phase.sin();
                left[i] = s;
                right[i] = s;
                phase = (phase + step) % std::f32::consts::TAU;
            }
            analyzer.push_block(&left, &right);
            let _ = analyzer.analyze();
        }
        *analyzer.levels()
    }

    #[test]
    fn silence_stays_on_the_floor() {
        let mut analyzer = SpectrumAnalyzer::new(48_000.0);
        let quiet = [0.0f32; 512];
        for _ in 0..8 {
            analyzer.push_block(&quiet, &quiet);
            let _ = analyzer.analyze();
        }
        assert!(analyzer.levels().iter().all(|db| *db <= FLOOR_DB + 1.0));
    }

    #[test]
    fn analysis_is_throttled_rather_than_run_per_block() {
        let mut analyzer = SpectrumAnalyzer::new(48_000.0);
        let block = [0.1f32; 256];
        // Under the threshold: no frame at all.
        analyzer.push_block(&block, &block);
        assert!(analyzer.analyze().is_none());
        // Past it: exactly one, then the counter resets.
        for _ in 0..(FRAMES_PER_ANALYSIS / 256) {
            analyzer.push_block(&block, &block);
        }
        assert!(analyzer.analyze().is_some());
        assert!(analyzer.analyze().is_none());
    }

    /// The published bins are a real measurement, so a tone has to land in the
    /// bin whose frequency range contains it — not merely "somewhere loud".
    #[test]
    fn a_tone_peaks_in_the_bin_that_covers_it() {
        let sample_rate = 48_000.0;
        for hz in [100.0f32, 1_000.0, 6_000.0] {
            let mut analyzer = SpectrumAnalyzer::new(sample_rate);
            let levels = analyze_tone(&mut analyzer, sample_rate, hz);

            let loudest = levels
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map(|(i, _)| i)
                .expect("levels are never empty");

            // Which output bin *should* own this tone, from the same log map.
            let expected =
                ((hz / MIN_HZ).ln() / (MAX_HZ / MIN_HZ).ln() * SPECTRUM_BINS as f32) as usize;
            let delta = loudest.abs_diff(expected);
            assert!(
                delta <= 2,
                "{hz} Hz peaked in bin {loudest}, expected ~{expected}"
            );
            // A full-scale sine must read near the top of the scale, proving
            // the normalisation is a real dBFS reading and not an arbitrary
            // window-dependent number.
            assert!(
                levels[loudest] > -6.0,
                "{hz} Hz read {} dB, expected near 0",
                levels[loudest]
            );
        }
    }

    /// Edges must stay monotonic and inside the transform at every rate — a
    /// bin reading backwards or past Nyquist would index garbage.
    #[test]
    fn edges_stay_monotonic_and_in_range_at_common_rates() {
        for sample_rate in [44_100.0f32, 48_000.0, 96_000.0, 192_000.0] {
            let analyzer = SpectrumAnalyzer::new(sample_rate);
            for i in 0..SPECTRUM_BINS {
                assert!(
                    analyzer.edges[i + 1] >= analyzer.edges[i],
                    "edges run backwards at bin {i}, {sample_rate} Hz"
                );
                assert!(analyzer.edges[i] >= 1);
                assert!(analyzer.edges[i + 1] <= FFT_SIZE / 2);
            }
        }
    }

    /// The bottom of the range is resolution-limited, so neighbouring output
    /// bins share an FFT bin there. That has to read as a repeated level, never
    /// as the floor — an empty range rendered as silence would draw a comb of
    /// false notches across the low end.
    #[test]
    fn resolution_limited_low_bins_repeat_rather_than_read_silent() {
        let sample_rate = 48_000.0;
        let mut analyzer = SpectrumAnalyzer::new(sample_rate);
        assert!(
            analyzer.edges[1] == analyzer.edges[0],
            "this test only means something while the low end is shared"
        );
        let levels = analyze_tone(&mut analyzer, sample_rate, 40.0);
        assert!(
            levels[0] > FLOOR_DB + 1.0,
            "bin 0 read the floor despite a zero-width range"
        );
    }

    #[test]
    fn quantisation_covers_the_documented_range() {
        assert_eq!(quantize_db(FLOOR_DB), 0);
        assert_eq!(quantize_db(CEIL_DB), 255);
        assert_eq!(quantize_db(FLOOR_DB - 50.0), 0, "clamps rather than wraps");
        assert_eq!(quantize_db(CEIL_DB + 50.0), 255);
        assert_eq!(quantize_db((FLOOR_DB + CEIL_DB) / 2.0), 128);
    }
}

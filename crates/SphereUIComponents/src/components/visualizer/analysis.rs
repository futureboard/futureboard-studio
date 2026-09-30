//! Signal analysis for the visualizers.
//!
//! Frames come from the engine's master tap
//! ([`DirectAudio::visualizer_tap`]) on the UI thread, at the
//! window's frame rate. Nothing here runs on the audio thread: the callback
//! only copies samples into the tap, and all of this — the FFT, the
//! correlation, the EBU R128 loudness — happens after the fact on whatever
//! the reader has collected since its last frame.
//!
//! Each window owns one [`Analyzer`] and asks it only for what its view draws
//! ([`Features`]), so a stereo-image window never runs an FFT and a spectrum
//! window never runs the loudness gates.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

/// Samples in one FFT: 8192 is 5.9 Hz a bin at 48 kHz, fine enough to
/// separate the lowest octave.
pub const FFT_SIZE: usize = 8_192;
/// Log-spaced display bins across [`MIN_HZ`]..[`MAX_HZ`], shared by the
/// spectrum curve and the spectrogram's rows.
pub const DISPLAY_BINS: usize = 256;
pub const MIN_HZ: f32 = 20.0;
pub const MAX_HZ: f32 = 20_000.0;
/// Spectrum scale after the slope, in dB.
pub const SPECTRUM_FLOOR_DB: f32 = -90.0;
pub const SPECTRUM_CEIL_DB: f32 = 0.0;
/// The spectrum is tilted by this much per octave around 1 kHz, so pink
/// noise — and most mixes — read flat.
pub const SLOPE_DB_PER_OCTAVE: f32 = 4.5;
/// Stereo pairs kept for the vectorscope.
pub const SCOPE_PAIRS: usize = 2_048;
/// Short-term loudness points kept for the history graph: one per
/// [`LOUDNESS_HISTORY_STEP`], thirty seconds in all.
pub const LOUDNESS_HISTORY: usize = 300;
pub const LOUDNESS_HISTORY_STEP: f32 = 0.1;
/// The oscilloscope's time span.
pub const SCOPE_WINDOW_SECONDS: f32 = 0.025;

/// Frames pulled from the tap per read.
const READ_BLOCK: usize = 4_096;
/// Integration time of the correlation, balance and RMS readings.
const METER_SECONDS: f32 = 0.3;
/// Spectrum ballistics.
const SPECTRUM_RISE_SECONDS: f32 = 0.03;
const SPECTRUM_FALL_SECONDS: f32 = 0.35;
/// A spectrum peak holds this long, then falls at [`PEAK_FALL_DB_PER_SECOND`].
const PEAK_HOLD_SECONDS: f32 = 1.0;
const PEAK_FALL_DB_PER_SECOND: f32 = 12.0;
/// A level peak readout holds this long before it follows the signal.
const LEVEL_HOLD_SECONDS: f32 = 1.5;
/// After this long without a frame the output is not running (device closed,
/// engine suspended), and the views decay as silence rather than freeze.
const IDLE_SECONDS: f32 = 0.25;
/// Below this mean-square energy a correlation or balance is not a reading.
const ENERGY_FLOOR: f32 = 1.0e-9;

/// What a window needs computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub spectrum: bool,
    pub loudness: bool,
}

/// One column for the spectrogram: the unsmoothed spectrum, 0..1 on the
/// display scale, lowest frequency first.
pub type SpectrogramColumn = [f32; DISPLAY_BINS];

pub struct Analyzer {
    features: Features,
    sample_rate: u32,
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
    /// Zeros fed in while the output is not running.
    silence: Vec<f32>,

    /// Mono history for the FFT and the oscilloscope; `mono_write` is the
    /// next slot.
    mono: Vec<f32>,
    mono_write: usize,
    /// Stereo history for the vectorscope.
    pairs: Vec<[f32; 2]>,
    pairs_write: usize,

    fft: Arc<dyn Fft<f32>>,
    fft_buffer: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    window: Vec<f32>,
    magnitudes: Vec<f32>,
    /// `(first, last)` FFT bin of each display bin, for the current rate.
    bin_ranges: Vec<(usize, usize)>,
    /// dB added to each display bin for the slope.
    bin_slope: Vec<f32>,
    raw: Vec<f32>,
    pub spectrum: Vec<f32>,
    pub peaks: Vec<f32>,
    peak_age: Vec<f32>,
    /// A new column since the last [`Analyzer::take_spectrogram_column`].
    column_fresh: bool,

    lr: f32,
    ll: f32,
    rr: f32,
    pub correlation: f32,
    /// `right − left` RMS in dB: positive leans right.
    pub balance_db: f32,
    pub rms: [f32; 2],
    pub peak: [f32; 2],
    peak_hold_age: [f32; 2],

    loudness: Option<ebur128::EbuR128>,
    pub momentary_lufs: Option<f32>,
    pub short_term_lufs: Option<f32>,
    pub integrated_lufs: Option<f32>,
    pub range_lu: Option<f32>,
    pub true_peak_db: Option<f32>,
    pub loudness_history: Vec<f32>,
    history_clock: f32,

    /// Whether any frame has arrived since the analyzer started or reset.
    pub live: bool,
    /// Seconds since the tap last delivered a frame.
    idle: f32,
}

impl Analyzer {
    pub fn new(features: Features) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let scratch = fft.get_inplace_scratch_len();
        let window = (0..FFT_SIZE)
            .map(|i| {
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (FFT_SIZE - 1) as f32).cos()
            })
            .collect();
        let mut analyzer = Self {
            features,
            sample_rate: 0,
            scratch_l: vec![0.0; READ_BLOCK],
            scratch_r: vec![0.0; READ_BLOCK],
            silence: vec![0.0; READ_BLOCK],
            mono: vec![0.0; FFT_SIZE],
            mono_write: 0,
            pairs: vec![[0.0; 2]; SCOPE_PAIRS],
            pairs_write: 0,
            fft,
            fft_buffer: vec![Complex::default(); FFT_SIZE],
            fft_scratch: vec![Complex::default(); scratch],
            window,
            magnitudes: vec![0.0; FFT_SIZE / 2],
            bin_ranges: vec![(0, 0); DISPLAY_BINS],
            bin_slope: vec![0.0; DISPLAY_BINS],
            raw: vec![SPECTRUM_FLOOR_DB; DISPLAY_BINS],
            spectrum: vec![SPECTRUM_FLOOR_DB; DISPLAY_BINS],
            peaks: vec![SPECTRUM_FLOOR_DB; DISPLAY_BINS],
            peak_age: vec![0.0; DISPLAY_BINS],
            column_fresh: false,
            lr: 0.0,
            ll: 0.0,
            rr: 0.0,
            correlation: 0.0,
            balance_db: 0.0,
            rms: [0.0; 2],
            peak: [0.0; 2],
            peak_hold_age: [0.0; 2],
            loudness: None,
            momentary_lufs: None,
            short_term_lufs: None,
            integrated_lufs: None,
            range_lu: None,
            true_peak_db: None,
            loudness_history: Vec::with_capacity(LOUDNESS_HISTORY),
            history_clock: 0.0,
            live: false,
            idle: 0.0,
        };
        analyzer.set_sample_rate(48_000);
        analyzer
    }

    pub fn features(&self) -> Features {
        self.features
    }

    pub fn set_features(&mut self, features: Features) {
        if features.loudness && !self.features.loudness {
            self.loudness = None;
            self.clear_loudness();
        }
        self.features = features;
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Frequency at the centre of display bin `index`.
    pub fn display_bin_hz(index: usize) -> f32 {
        let t = (index as f32 + 0.5) / DISPLAY_BINS as f32;
        MIN_HZ * (MAX_HZ / MIN_HZ).powf(t)
    }

    fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate == 0 || sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        let hz_per_bin = sample_rate as f32 / FFT_SIZE as f32;
        let last_bin = FFT_SIZE / 2 - 1;
        for index in 0..DISPLAY_BINS {
            let lo = MIN_HZ * (MAX_HZ / MIN_HZ).powf(index as f32 / DISPLAY_BINS as f32);
            let hi = MIN_HZ * (MAX_HZ / MIN_HZ).powf((index + 1) as f32 / DISPLAY_BINS as f32);
            let first = ((lo / hz_per_bin).round() as usize).clamp(1, last_bin);
            let last = ((hi / hz_per_bin).round() as usize).clamp(first, last_bin);
            self.bin_ranges[index] = (first, last);
            let centre = Self::display_bin_hz(index);
            self.bin_slope[index] = SLOPE_DB_PER_OCTAVE * (centre / 1_000.0).log2();
        }
        if self.loudness.is_some() {
            self.loudness = None;
            self.clear_loudness();
        }
    }

    fn clear_loudness(&mut self) {
        self.momentary_lufs = None;
        self.short_term_lufs = None;
        self.integrated_lufs = None;
        self.range_lu = None;
        self.true_peak_db = None;
        self.loudness_history.clear();
        self.history_clock = 0.0;
    }

    /// Start the integrated reading, range and true peak over.
    pub fn reset_loudness(&mut self) {
        if let Some(meter) = self.loudness.as_mut() {
            meter.reset();
        }
        self.clear_loudness();
    }

    /// Pull whatever the tap collected since `cursor`, analyse it, and return
    /// the new cursor. `dt` is the time since the previous call.
    pub fn pull(&mut self, cursor: u64, dt: f32) -> u64 {
        let tap = DirectAudio::visualizer_tap();
        self.set_sample_rate(tap.sample_rate());
        let mut cursor = cursor;
        let mut received = 0;
        // At most a few reads a frame: a window that was hidden for a while
        // resynchronises through the tap's own skip rather than catching up.
        for _ in 0..4 {
            let mut left = std::mem::take(&mut self.scratch_l);
            let mut right = std::mem::take(&mut self.scratch_r);
            let read = tap.read(cursor, &mut left, &mut right);
            cursor = read.next;
            received += read.frames;
            if read.frames > 0 {
                self.push(&left[..read.frames], &right[..read.frames]);
            }
            self.scratch_l = left;
            self.scratch_r = right;
            if read.frames < READ_BLOCK {
                break;
            }
        }
        if received > 0 {
            self.idle = 0.0;
        } else {
            self.idle += dt;
            if self.idle > IDLE_SECONDS && self.live {
                let frames = ((self.sample_rate as f32 * dt) as usize).min(READ_BLOCK);
                let silence = std::mem::take(&mut self.silence);
                self.push(&silence[..frames], &silence[..frames]);
                self.silence = silence;
            }
        }
        self.finish_frame(dt);
        cursor
    }

    /// Feed a block of stereo frames. `pull` does this from the tap; tests
    /// call it directly.
    pub fn push(&mut self, left: &[f32], right: &[f32]) {
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }
        self.live = true;
        let coeff = (-1.0 / (self.sample_rate.max(1) as f32 * METER_SECONDS)).exp();
        let take = 1.0 - coeff;
        for i in 0..frames {
            let (l, r) = (left[i], right[i]);
            self.mono[self.mono_write] = 0.5 * (l + r);
            self.mono_write = (self.mono_write + 1) % FFT_SIZE;
            self.pairs[self.pairs_write] = [l, r];
            self.pairs_write = (self.pairs_write + 1) % SCOPE_PAIRS;
            self.lr = flush(coeff * self.lr + take * l * r);
            self.ll = flush(coeff * self.ll + take * l * l);
            self.rr = flush(coeff * self.rr + take * r * r);
            let peaks = [l.abs(), r.abs()];
            for side in 0..2 {
                if peaks[side] >= self.peak[side] {
                    self.peak[side] = peaks[side];
                    self.peak_hold_age[side] = 0.0;
                }
            }
        }
        if self.features.loudness {
            if self.loudness.is_none() {
                let mode = ebur128::Mode::I
                    | ebur128::Mode::S
                    | ebur128::Mode::LRA
                    | ebur128::Mode::TRUE_PEAK
                    | ebur128::Mode::HISTOGRAM;
                self.loudness = ebur128::EbuR128::new(2, self.sample_rate.max(1), mode).ok();
            }
            if let Some(meter) = self.loudness.as_mut() {
                let _ = meter.add_frames_planar_f32(&[&left[..frames], &right[..frames]]);
            }
        }
    }

    fn finish_frame(&mut self, dt: f32) {
        let dt = dt.clamp(0.0, 0.5);
        let energy = self.ll * self.rr;
        self.correlation = if energy > ENERGY_FLOOR * ENERGY_FLOOR {
            (self.lr / energy.sqrt()).clamp(-1.0, 1.0)
        } else {
            0.0
        };
        self.rms = [self.ll.max(0.0).sqrt(), self.rr.max(0.0).sqrt()];
        self.balance_db = if self.ll > ENERGY_FLOOR && self.rr > ENERGY_FLOOR {
            10.0 * (self.rr / self.ll).log10()
        } else {
            0.0
        };
        for side in 0..2 {
            self.peak_hold_age[side] += dt;
            if self.peak_hold_age[side] > LEVEL_HOLD_SECONDS {
                // Release towards the RMS rather than dropping to nothing.
                self.peak[side] = (self.peak[side] * (1.0 - dt * 3.0)).max(self.rms[side]);
            }
        }
        if self.features.spectrum {
            self.analyse_spectrum(dt);
        }
        if self.features.loudness {
            self.read_loudness(dt);
        }
    }

    fn analyse_spectrum(&mut self, dt: f32) {
        // Oldest first, windowed.
        for i in 0..FFT_SIZE {
            let sample = self.mono[(self.mono_write + i) % FFT_SIZE];
            self.fft_buffer[i] = Complex::new(sample * self.window[i], 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);
        // A full-scale sine reads 0 dB: Hann's coherent gain is one half.
        let scale = 2.0 / (FFT_SIZE as f32 * 0.5);
        for (bin, value) in self.magnitudes.iter_mut().enumerate() {
            *value = self.fft_buffer[bin].norm() * scale;
        }
        let rise = 1.0 - (-dt / SPECTRUM_RISE_SECONDS).exp();
        let fall = 1.0 - (-dt / SPECTRUM_FALL_SECONDS).exp();
        for index in 0..DISPLAY_BINS {
            let (first, last) = self.bin_ranges[index];
            let magnitude = self.magnitudes[first..=last]
                .iter()
                .fold(0.0f32, |peak, value| peak.max(*value));
            let db = (20.0 * magnitude.max(1.0e-9).log10() + self.bin_slope[index])
                .clamp(SPECTRUM_FLOOR_DB, SPECTRUM_CEIL_DB + 12.0);
            self.raw[index] = db;
            let current = self.spectrum[index];
            let rate = if db > current { rise } else { fall };
            self.spectrum[index] = current + (db - current) * rate;
            if self.spectrum[index] >= self.peaks[index] {
                self.peaks[index] = self.spectrum[index];
                self.peak_age[index] = 0.0;
            } else {
                self.peak_age[index] += dt;
                if self.peak_age[index] > PEAK_HOLD_SECONDS {
                    self.peaks[index] = (self.peaks[index] - PEAK_FALL_DB_PER_SECOND * dt)
                        .max(self.spectrum[index]);
                }
            }
        }
        self.column_fresh = true;
    }

    /// The newest spectrogram column, once per analysed frame.
    pub fn take_spectrogram_column(&mut self) -> Option<SpectrogramColumn> {
        if !self.column_fresh {
            return None;
        }
        self.column_fresh = false;
        let mut column = [0.0; DISPLAY_BINS];
        for (value, db) in column.iter_mut().zip(&self.raw) {
            *value =
                ((db - SPECTRUM_FLOOR_DB) / (SPECTRUM_CEIL_DB - SPECTRUM_FLOOR_DB)).clamp(0.0, 1.0);
        }
        Some(column)
    }

    fn read_loudness(&mut self, dt: f32) {
        let Some(meter) = self.loudness.as_ref() else {
            return;
        };
        let finite = |value: Result<f64, ebur128::Error>| {
            value
                .ok()
                .map(|v| v as f32)
                .filter(|v| v.is_finite() && *v > -200.0)
        };
        self.momentary_lufs = finite(meter.loudness_momentary());
        self.short_term_lufs = finite(meter.loudness_shortterm());
        self.integrated_lufs = finite(meter.loudness_global());
        self.range_lu = meter
            .loudness_range()
            .ok()
            .map(|v| v as f32)
            .filter(|v| v.is_finite());
        let true_peak = (0..2)
            .filter_map(|channel| meter.true_peak(channel).ok())
            .fold(0.0f64, f64::max);
        self.true_peak_db = (true_peak > 0.0).then(|| 20.0 * (true_peak as f32).log10());

        self.history_clock += dt;
        while self.history_clock >= LOUDNESS_HISTORY_STEP {
            self.history_clock -= LOUDNESS_HISTORY_STEP;
            if self.loudness_history.len() == LOUDNESS_HISTORY {
                self.loudness_history.remove(0);
            }
            self.loudness_history
                .push(self.short_term_lufs.unwrap_or(f32::NEG_INFINITY));
        }
    }

    /// The last [`SCOPE_PAIRS`] stereo frames, oldest first.
    pub fn scope_pairs(&self) -> impl Iterator<Item = [f32; 2]> + '_ {
        (0..SCOPE_PAIRS).map(move |i| self.pairs[(self.pairs_write + i) % SCOPE_PAIRS])
    }

    /// The oscilloscope trace: the newest [`SCOPE_WINDOW_SECONDS`] of the mono
    /// signal, started on a rising zero crossing when there is one, so a
    /// steady tone stands still instead of crawling.
    pub fn oscilloscope(&self, out: &mut Vec<f32>) {
        out.clear();
        let span = ((self.sample_rate.max(1) as f32 * SCOPE_WINDOW_SECONDS) as usize)
            .clamp(64, FFT_SIZE / 2);
        let newest = (self.mono_write + FFT_SIZE - 1) % FFT_SIZE;
        let at = |age: usize| self.mono[(newest + FFT_SIZE - age) % FFT_SIZE];
        // Search the half-window before the span for the latest rising crossing.
        let mut start_age = span;
        for age in span..(span * 2).min(FFT_SIZE - 1) {
            if at(age + 1) < 0.0 && at(age) >= 0.0 {
                start_age = age;
                break;
            }
        }
        for offset in 0..span {
            out.push(at(start_age - offset));
        }
    }
}

#[inline]
fn flush(value: f32) -> f32 {
    if value.abs() < 1.0e-25 { 0.0 } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn sine(hz: f32, amplitude: f32, frames: usize, phase_right: f32) -> (Vec<f32>, Vec<f32>) {
        let left = (0..frames)
            .map(|i| amplitude * (std::f32::consts::TAU * hz * i as f32 / RATE).sin())
            .collect();
        let right = (0..frames)
            .map(|i| amplitude * (std::f32::consts::TAU * hz * i as f32 / RATE + phase_right).sin())
            .collect();
        (left, right)
    }

    fn run(analyzer: &mut Analyzer, left: &[f32], right: &[f32]) {
        for (l, r) in left.chunks(800).zip(right.chunks(800)) {
            analyzer.push(l, r);
            analyzer.finish_frame(1.0 / 60.0);
        }
    }

    #[test]
    fn a_full_scale_sine_peaks_at_its_bin_near_zero_db() {
        let mut analyzer = Analyzer::new(Features {
            spectrum: true,
            loudness: false,
        });
        let (l, r) = sine(1_000.0, 1.0, 48_000, 0.0);
        run(&mut analyzer, &l, &r);
        let (index, db) =
            analyzer
                .spectrum
                .iter()
                .copied()
                .enumerate()
                .fold(
                    (0, f32::MIN),
                    |best, (i, v)| if v > best.1 { (i, v) } else { best },
                );
        let hz = Analyzer::display_bin_hz(index);
        assert!((hz / 1_000.0).log2().abs() < 0.1, "peak at {hz} Hz");
        // The slope is 0 dB at 1 kHz.
        assert!(db.abs() < 1.5, "{db} dB");
        let far = analyzer.spectrum[10];
        assert!(far < -60.0, "{far} dB far from the tone");
    }

    #[test]
    fn correlation_reads_mono_inverted_and_quadrature() {
        let reading = |phase: f32| {
            let mut analyzer = Analyzer::new(Features::default());
            let (l, r) = sine(440.0, 0.5, 48_000, phase);
            run(&mut analyzer, &l, &r);
            analyzer.correlation
        };
        assert!(reading(0.0) > 0.98);
        assert!(reading(std::f32::consts::PI) < -0.98);
        assert!(reading(std::f32::consts::FRAC_PI_2).abs() < 0.05);
        let silent = Analyzer::new(Features::default());
        assert_eq!(silent.correlation, 0.0);
    }

    #[test]
    fn balance_leans_towards_the_louder_side() {
        let mut analyzer = Analyzer::new(Features::default());
        let (l, _) = sine(440.0, 0.25, 48_000, 0.0);
        let (r, _) = sine(440.0, 0.5, 48_000, 0.0);
        run(&mut analyzer, &l, &r);
        assert!(
            (analyzer.balance_db - 6.02).abs() < 0.2,
            "{}",
            analyzer.balance_db
        );
    }

    /// EBU R128 (Tech 3341): a 1 kHz sine at 0 dBFS in one channel reads
    /// −3.01 LUFS, so the same sine in both channels at −18 dBFS reads
    /// −18.0 LUFS; its true peak is −18 dBTP.
    #[test]
    fn loudness_of_a_steady_tone_is_stable_and_near_the_reference() {
        let mut analyzer = Analyzer::new(Features {
            spectrum: false,
            loudness: true,
        });
        let amplitude = 10f32.powf(-18.0 / 20.0);
        let (l, r) = sine(1_000.0, amplitude, 48_000 * 6, 0.0);
        run(&mut analyzer, &l, &r);
        let integrated = analyzer.integrated_lufs.expect("integrated after 6 s");
        let short = analyzer.short_term_lufs.expect("short-term after 3 s");
        assert!((integrated + 18.0).abs() < 0.3, "{integrated} LUFS");
        assert!((short - integrated).abs() < 0.3);
        let peak = analyzer.true_peak_db.expect("true peak");
        assert!((peak + 18.0).abs() < 0.5, "{peak} dBTP");
        assert!(analyzer.loudness_history.len() > 30);
        analyzer.reset_loudness();
        assert!(analyzer.integrated_lufs.is_none());
    }

    #[test]
    fn the_oscilloscope_starts_on_a_rising_zero_crossing() {
        let mut analyzer = Analyzer::new(Features::default());
        let (l, r) = sine(200.0, 0.8, 8_000, 0.0);
        run(&mut analyzer, &l, &r);
        let mut trace = Vec::new();
        analyzer.oscilloscope(&mut trace);
        assert_eq!(trace.len(), (RATE * SCOPE_WINDOW_SECONDS) as usize);
        assert!(
            trace[0].abs() < 0.05 && trace[3] > trace[0],
            "{:?}",
            &trace[..4]
        );
    }

    #[test]
    fn a_spectrogram_column_comes_once_per_analysis() {
        let mut analyzer = Analyzer::new(Features {
            spectrum: true,
            loudness: false,
        });
        assert!(analyzer.take_spectrogram_column().is_none());
        let (l, r) = sine(100.0, 0.5, 4_800, 0.0);
        run(&mut analyzer, &l, &r);
        let column = analyzer.take_spectrogram_column().expect("a column");
        assert!(column.iter().all(|v| (0.0..=1.0).contains(v)));
        assert!(analyzer.take_spectrogram_column().is_none());
    }
}

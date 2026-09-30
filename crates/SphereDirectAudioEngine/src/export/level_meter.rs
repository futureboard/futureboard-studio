//! Level report for one rendered file: sample peak, true peak, integrated
//! loudness and how much of it went over full scale.
//!
//! Fed the exact samples handed to the encoder, so it describes the file,
//! and before an integer format clamps them, so it can say that it did. Runs
//! on the export (or capture writer) thread, never the audio callback.

use serde::{Deserialize, Serialize};

/// What a rendered file's levels came to.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LevelReport {
    /// Highest absolute sample, dBFS. `None` for silence.
    pub sample_peak_db: Option<f32>,
    /// Highest inter-sample peak (4× oversampled, ITU-R BS.1770), dBTP.
    /// `None` for silence or a measurement that could not run.
    pub true_peak_db: Option<f32>,
    /// Integrated loudness, LUFS. `None` when the file is too short or too
    /// quiet to gate.
    pub integrated_lufs: Option<f32>,
    /// Samples (per channel, counted individually) above full scale.
    pub clipped_samples: u64,
    /// Where the first sample above full scale is, in seconds from the start
    /// of the file.
    pub first_clip_seconds: Option<f64>,
}

impl LevelReport {
    /// Any sample went over 0 dBFS.
    pub fn clips(&self) -> bool {
        self.clipped_samples > 0
    }

    /// The inter-sample peak exceeds 0 dBTP although no sample did: a
    /// converter or a lossy encoder can still clip it on playback.
    pub fn true_peak_over(&self) -> bool {
        self.true_peak_db.is_some_and(|tp| tp > 0.0)
    }
}

/// Streaming meter behind [`LevelReport`].
pub struct LevelMeter {
    channels: usize,
    sample_rate: u32,
    loudness: Option<ebur128::EbuR128>,
    peak: f32,
    clipped: u64,
    first_clip_frame: Option<u64>,
    frames: u64,
}

impl LevelMeter {
    pub fn new(channels: u16, sample_rate: u32) -> Self {
        let channels = usize::from(channels.max(1));
        let loudness = ebur128::EbuR128::new(
            channels as u32,
            sample_rate.max(1),
            ebur128::Mode::I | ebur128::Mode::TRUE_PEAK,
        )
        .ok();
        Self {
            channels,
            sample_rate: sample_rate.max(1),
            loudness,
            peak: 0.0,
            clipped: 0,
            first_clip_frame: None,
            frames: 0,
        }
    }

    /// Measure the next interleaved block.
    pub fn feed(&mut self, interleaved: &[f32]) {
        for (index, sample) in interleaved.iter().enumerate() {
            let level = sample.abs();
            if level > self.peak {
                self.peak = level;
            }
            if level > 1.0 {
                self.clipped += 1;
                if self.first_clip_frame.is_none() {
                    self.first_clip_frame = Some(self.frames + (index / self.channels) as u64);
                }
            }
        }
        if let Some(loudness) = self.loudness.as_mut() {
            if loudness.add_frames_f32(interleaved).is_err() {
                self.loudness = None;
            }
        }
        self.frames += (interleaved.len() / self.channels) as u64;
    }

    pub fn finish(self) -> LevelReport {
        let to_db = |linear: f64| (linear > 1.0e-9).then(|| (20.0 * linear.log10()) as f32);
        let (true_peak_db, integrated_lufs) = match self.loudness.as_ref() {
            Some(loudness) => {
                let true_peak = (0..self.channels as u32)
                    .filter_map(|channel| loudness.true_peak(channel).ok())
                    .fold(0.0_f64, f64::max)
                    // The oversampled peak is never below the sample peak; a
                    // short file can leave the filter short of it.
                    .max(f64::from(self.peak));
                let integrated = loudness
                    .loudness_global()
                    .ok()
                    .filter(|lufs| lufs.is_finite())
                    .map(|lufs| lufs as f32);
                (to_db(true_peak), integrated)
            }
            None => (to_db(f64::from(self.peak)), None),
        };
        LevelReport {
            sample_peak_db: to_db(f64::from(self.peak)),
            true_peak_db,
            integrated_lufs,
            clipped_samples: self.clipped,
            first_clip_seconds: self
                .first_clip_frame
                .map(|frame| frame as f64 / f64::from(self.sample_rate)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amplitude: f32, frames: usize, rate: u32) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v =
                    amplitude * (2.0 * std::f32::consts::PI * 997.0 * i as f32 / rate as f32).sin();
                [v, v]
            })
            .collect()
    }

    #[test]
    fn a_file_inside_full_scale_does_not_clip() {
        let mut meter = LevelMeter::new(2, 48_000);
        meter.feed(&sine(0.5, 48_000, 48_000));
        let report = meter.finish();
        assert!(!report.clips());
        assert!((report.sample_peak_db.unwrap() - (-6.02)).abs() < 0.1);
        assert!(report.integrated_lufs.is_some());
    }

    #[test]
    fn overs_are_counted_and_located() {
        let mut meter = LevelMeter::new(2, 48_000);
        let mut quiet = sine(0.25, 24_000, 48_000);
        meter.feed(&quiet);
        quiet.iter_mut().for_each(|s| *s *= 6.0); // peaks at 1.5
        meter.feed(&quiet);
        let report = meter.finish();
        assert!(report.clips());
        assert!(report.sample_peak_db.unwrap() > 3.0);
        let first = report.first_clip_seconds.unwrap();
        assert!((0.5..0.51).contains(&first), "first clip at {first}");
    }

    #[test]
    fn silence_reports_no_levels() {
        let mut meter = LevelMeter::new(1, 44_100);
        meter.feed(&vec![0.0; 44_100]);
        let report = meter.finish();
        assert_eq!(report.sample_peak_db, None);
        assert!(!report.clips());
    }
}

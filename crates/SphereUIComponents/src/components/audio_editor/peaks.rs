//! Per-channel min/max pyramid over the clip's decoded PCM.
//!
//! The arrangement's waveform cache is a mono downmix sized for thumbnails.
//! An editor needs each channel separately and every zoom from the whole file
//! down to single samples, so it keeps its own pyramid over the PCM it already
//! holds for editing: level 0 summarises [`BASE_FRAMES`] frames per bin, each
//! level above halves the bins. Any range is answered from the coarsest level
//! that still has several bins inside it, and ranges shorter than a base bin
//! read the samples themselves. Built once per source on a background thread;
//! queries are allocation-free.

/// Frames summarised by one level-0 bin.
pub const BASE_FRAMES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MinMax {
    pub min: f32,
    pub max: f32,
}

impl MinMax {
    const EMPTY: MinMax = MinMax {
        min: f32::INFINITY,
        max: f32::NEG_INFINITY,
    };

    fn add(&mut self, v: f32) {
        if v.is_finite() {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
    }

    fn merge(&mut self, other: MinMax) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    /// `None` for a range that held no finite sample.
    fn finish(self) -> Option<MinMax> {
        (self.min <= self.max).then_some(self)
    }
}

#[derive(Debug)]
pub struct PeakPyramid {
    channels: usize,
    frames: usize,
    /// `levels[k][bin * channels + ch]`, `BASE_FRAMES << k` frames per bin.
    levels: Vec<Vec<MinMax>>,
}

impl PeakPyramid {
    pub fn build(samples: &[f32], channels: usize) -> Self {
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        let bins = frames.div_ceil(BASE_FRAMES);
        let mut base = vec![MinMax::EMPTY; bins * channels];
        for (frame, chunk) in samples.chunks_exact(channels).enumerate() {
            let bin = frame / BASE_FRAMES;
            for (ch, v) in chunk.iter().enumerate() {
                base[bin * channels + ch].add(*v);
            }
        }
        let mut levels = vec![base];
        while levels.last().map_or(0, |l| l.len() / channels) > 1 {
            let below = levels.last().unwrap();
            let below_bins = below.len() / channels;
            let mut next = vec![MinMax::EMPTY; below_bins.div_ceil(2) * channels];
            for bin in 0..below_bins {
                for ch in 0..channels {
                    next[(bin / 2) * channels + ch].merge(below[bin * channels + ch]);
                }
            }
            levels.push(next);
        }
        Self {
            channels,
            frames,
            levels,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Min/max of channel `ch` over frames `[start, end)`. `samples` must be
    /// the PCM the pyramid was built from; short ranges read it directly.
    pub fn range(&self, samples: &[f32], ch: usize, start: usize, end: usize) -> Option<MinMax> {
        let end = end.min(self.frames);
        if start >= end || ch >= self.channels {
            return None;
        }
        let span = end - start;
        if span <= BASE_FRAMES * 2 {
            let mut acc = MinMax::EMPTY;
            for frame in start..end {
                acc.add(samples[frame * self.channels + ch]);
            }
            return acc.finish();
        }
        // Coarsest level with at least ~8 bins in the range: a partially
        // covered bin at either end widens the answer by less than one bin,
        // which is below a pixel at the zoom that picked this level.
        let mut level = 0;
        while level + 1 < self.levels.len() && (BASE_FRAMES << (level + 1)) * 8 <= span {
            level += 1;
        }
        let bin_frames = BASE_FRAMES << level;
        let data = &self.levels[level];
        let first = start / bin_frames;
        let last = (end - 1) / bin_frames;
        let mut acc = MinMax::EMPTY;
        for bin in first..=last {
            acc.merge(data[bin * self.channels + ch]);
        }
        acc.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_match_a_direct_scan() {
        let frames = 10_000;
        let samples: Vec<f32> = (0..frames * 2)
            .map(|i| ((i as f32) * 0.013).sin() * if i % 2 == 0 { 1.0 } else { 0.5 })
            .collect();
        let pyramid = PeakPyramid::build(&samples, 2);
        for &(a, b) in &[(0, 5), (3, 40), (100, 9_999), (0, 10_000), (4_321, 4_400)] {
            for ch in 0..2 {
                let got = pyramid.range(&samples, ch, a, b).unwrap();
                let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
                for f in a..b {
                    lo = lo.min(samples[f * 2 + ch]);
                    hi = hi.max(samples[f * 2 + ch]);
                }
                // Level answers may include up to one bin beyond the range,
                // never less than the range itself.
                assert!(got.min <= lo + 1e-6 && got.max >= hi - 1e-6);
                assert!((got.max - hi).abs() < 0.15 && (got.min - lo).abs() < 0.15);
            }
        }
    }

    #[test]
    fn channels_stay_separate() {
        let samples = vec![1.0f32, -0.25].repeat(1_000);
        let pyramid = PeakPyramid::build(&samples, 2);
        let left = pyramid.range(&samples, 0, 0, 1_000).unwrap();
        let right = pyramid.range(&samples, 1, 0, 1_000).unwrap();
        assert_eq!((left.min, left.max), (1.0, 1.0));
        assert_eq!((right.min, right.max), (-0.25, -0.25));
    }

    #[test]
    fn empty_and_out_of_range_queries_are_none() {
        let pyramid = PeakPyramid::build(&[0.5; 64], 1);
        assert!(pyramid.range(&[0.5; 64], 0, 10, 10).is_none());
        assert!(pyramid.range(&[0.5; 64], 1, 0, 10).is_none());
        assert!(pyramid.range(&[0.5; 64], 0, 70, 90).is_none());
    }
}

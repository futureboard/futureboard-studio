//! A decoded sample, ready for the voices to read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::SystemTime;

/// One decoded sample: up to two planes at the file's own rate. Immutable once
/// built and shared by `Arc`, so a project rebuild that keeps the same file
/// keeps the same audio without reading it again.
#[derive(Debug)]
pub struct SampleData {
    sample_rate: u32,
    frames: usize,
    left: Box<[f32]>,
    /// Empty for a mono sample: both outputs read `left`.
    right: Box<[f32]>,
    peak: f32,
}

impl SampleData {
    /// Builds a sample from interleaved frames. A file with more than two
    /// channels keeps its first two. `None` for an empty or unusable buffer.
    pub fn from_interleaved(samples: &[f32], channels: usize, sample_rate: u32) -> Option<Self> {
        if channels == 0 || sample_rate == 0 {
            return None;
        }
        let frames = samples.len() / channels;
        if frames == 0 {
            return None;
        }
        let clean = |value: f32| if value.is_finite() { value } else { 0.0 };
        let left: Box<[f32]> = (0..frames).map(|i| clean(samples[i * channels])).collect();
        let right: Box<[f32]> = if channels >= 2 {
            (0..frames)
                .map(|i| clean(samples[i * channels + 1]))
                .collect()
        } else {
            Box::default()
        };
        let peak = left
            .iter()
            .chain(right.iter())
            .fold(0.0_f32, |peak, value| peak.max(value.abs()));
        Some(Self {
            sample_rate,
            frames,
            left,
            right,
            peak,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn channels(&self) -> usize {
        if self.right.is_empty() { 1 } else { 2 }
    }

    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }

    /// Largest absolute sample value, for normalizing.
    pub fn peak(&self) -> f32 {
        self.peak
    }

    #[inline]
    pub(crate) fn left(&self) -> &[f32] {
        &self.left
    }

    #[inline]
    pub(crate) fn right(&self) -> &[f32] {
        if self.right.is_empty() {
            &self.left
        } else {
            &self.right
        }
    }

    /// Both channels mixed to one, for analysing the audio (finding its hits
    /// or its tempo). Control thread only.
    pub fn mono(&self) -> Vec<f32> {
        self.left()
            .iter()
            .zip(self.right())
            .map(|(l, r)| 0.5 * (l + r))
            .collect()
    }

    /// `(min, max)` of both channels over `buckets` equal slices of the
    /// sample, for drawing its waveform. Control thread only.
    pub fn peaks(&self, buckets: usize) -> Vec<(f32, f32)> {
        let buckets = buckets.max(1);
        (0..buckets)
            .map(|bucket| {
                let from = bucket * self.frames / buckets;
                let to = ((bucket + 1) * self.frames / buckets)
                    .max(from + 1)
                    .min(self.frames);
                let mut low = f32::MAX;
                let mut high = f32::MIN;
                for i in from..to {
                    for value in [self.left[i], self.right()[i]] {
                        low = low.min(value);
                        high = high.max(value);
                    }
                }
                if low > high { (0.0, 0.0) } else { (low, high) }
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SampleKey {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

static CACHE: OnceLock<Mutex<HashMap<SampleKey, Weak<SampleData>>>> = OnceLock::new();

/// The sample at `path`, decoded by `decode` unless a live copy of the same
/// file (same length and modification time) is already loaded.
///
/// Control and offline threads only: it reads file metadata and may decode.
/// Must never be called from an audio callback.
pub fn load_cached(
    path: &Path,
    decode: impl FnOnce(&Path) -> Result<SampleData, String>,
) -> Result<Arc<SampleData>, String> {
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    let key = SampleKey {
        path: path.to_path_buf(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    };
    let cache = CACHE.get_or_init(Default::default);
    if let Some(live) = cache
        .lock()
        .ok()
        .and_then(|map| map.get(&key).and_then(Weak::upgrade))
    {
        return Ok(live);
    }
    let sample = Arc::new(decode(path)?);
    if let Ok(mut map) = cache.lock() {
        map.retain(|_, weak| weak.strong_count() > 0);
        map.insert(key, Arc::downgrade(&sample));
    }
    Ok(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_stereo_splits_into_planes() {
        let sample = SampleData::from_interleaved(&[0.1, -0.2, 0.3, -0.9], 2, 48_000).unwrap();
        assert_eq!(sample.frames(), 2);
        assert_eq!(sample.channels(), 2);
        assert_eq!(sample.left(), [0.1, 0.3]);
        assert_eq!(sample.right(), [-0.2, -0.9]);
        assert_eq!(sample.peak(), 0.9);
    }

    #[test]
    fn mono_reads_the_same_plane_on_both_sides() {
        let sample = SampleData::from_interleaved(&[0.5, 0.25], 1, 44_100).unwrap();
        assert_eq!(sample.channels(), 1);
        assert_eq!(sample.left(), sample.right());
    }

    #[test]
    fn peaks_cover_the_whole_sample() {
        let samples: Vec<f32> = (0..100).map(|i| if i == 99 { -1.0 } else { 0.0 }).collect();
        let sample = SampleData::from_interleaved(&samples, 1, 48_000).unwrap();
        let peaks = sample.peaks(10);
        assert_eq!(peaks.len(), 10);
        assert_eq!(peaks[9], (-1.0, 0.0));
    }
}

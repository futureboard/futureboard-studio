//! DC offset measurement and constant subtraction.

use super::AudioClipProcessor;
#[cfg(test)]
use super::peak_amplitude;

/// Per-channel DC estimate in linear amplitude.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DcOffset {
    pub left: f32,
    pub right: f32,
}

impl DcOffset {
    pub fn is_negligible(self) -> bool {
        self.left.abs() < 1.0e-6 && self.right.abs() < 1.0e-6
    }
}

/// Mean of each channel over interleaved PCM.
pub fn measure_dc_offset(samples: &[f32], channels: usize) -> DcOffset {
    if samples.is_empty() || channels == 0 {
        return DcOffset::default();
    }
    let frames = samples.len() / channels;
    if frames == 0 {
        return DcOffset::default();
    }
    let mut left = 0.0_f64;
    let mut right = 0.0_f64;
    for frame in samples.chunks(channels) {
        left += frame.first().copied().unwrap_or(0.0) as f64;
        right += if channels > 1 {
            frame[1] as f64
        } else {
            frame[0] as f64
        };
    }
    let n = frames as f64;
    DcOffset {
        left: (left / n) as f32,
        right: (right / n) as f32,
    }
}

/// Subtracts a constant DC estimate. Realtime-safe after construction.
#[derive(Debug, Clone, Copy)]
pub struct DcOffsetProcessor {
    offset: DcOffset,
    enabled: bool,
}

impl DcOffsetProcessor {
    pub fn new(offset: DcOffset, enabled: bool) -> Self {
        Self { offset, enabled }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_offset(&mut self, offset: DcOffset) {
        self.offset = offset;
    }

    #[inline]
    pub fn process_stereo(&self, left: f32, right: f32) -> (f32, f32) {
        if !self.enabled {
            return (left, right);
        }
        (left - self.offset.left, right - self.offset.right)
    }
}

impl AudioClipProcessor for DcOffsetProcessor {
    fn prepare(&mut self, _sample_rate: u32, _channels: usize, _max_block_frames: usize) {}

    fn reset(&mut self) {}

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let n = input.len().min(output.len());
        if !self.enabled {
            output[..n].copy_from_slice(&input[..n]);
            return;
        }
        for (i, sample) in input.iter().take(n).enumerate() {
            let dc = if i % 2 == 0 {
                self.offset.left
            } else {
                self.offset.right
            };
            output[i] = *sample - dc;
        }
    }
}

/// Peak remaining after DC removal. Tests only; nothing reads it out.
#[cfg(test)]
pub fn residual_peak_after_dc(samples: &[f32], _channels: usize, offset: DcOffset) -> f32 {
    let mut processor = DcOffsetProcessor::new(offset, true);
    let mut out = vec![0.0; samples.len()];
    processor.process(samples, &mut out);
    peak_amplitude(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_and_removes_a_constant_bias() {
        let mut samples = Vec::new();
        for i in 0..256 {
            let t = i as f32 / 256.0;
            samples.push((t * std::f32::consts::TAU).sin() + 0.2);
            samples.push((t * std::f32::consts::TAU).cos() - 0.15);
        }
        let dc = measure_dc_offset(&samples, 2);
        assert!((dc.left - 0.2).abs() < 0.02);
        assert!((dc.right + 0.15).abs() < 0.02);
        let residual = residual_peak_after_dc(&samples, 2, dc);
        assert!(residual < 1.05);
        let mut processor = DcOffsetProcessor::new(dc, true);
        let mut out = vec![0.0; samples.len()];
        processor.process(&samples, &mut out);
        let after = measure_dc_offset(&out, 2);
        assert!(after.left.abs() < 0.005);
        assert!(after.right.abs() < 0.005);
    }
}

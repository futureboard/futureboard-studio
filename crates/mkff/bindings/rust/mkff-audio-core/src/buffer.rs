/// Decoded audio: planar f32 in [-1, 1], one `Vec` per channel, all the same length. Surround
/// layouts are in WAV / SMPTE order (L R C LFE Ls Rs …).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
}

impl AudioBuffer {
    pub fn new(sample_rate: u32, channels: usize, frames: usize) -> Self {
        Self {
            sample_rate,
            channels: vec![vec![0.0; frames]; channels],
        }
    }

    pub fn empty(sample_rate: u32, channels: usize) -> Self {
        Self::new(sample_rate, channels, 0)
    }

    /// Samples per channel.
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames() == 0
    }

    /// Interleaved samples (L R L R … for stereo).
    pub fn interleaved(&self) -> Vec<f32> {
        let (n, ch) = (self.frames(), self.channels.len());
        let mut out = Vec::with_capacity(n * ch);
        for i in 0..n {
            for c in &self.channels {
                out.push(c.get(i).copied().unwrap_or(0.0));
            }
        }
        out
    }

    /// From interleaved samples.
    pub fn from_interleaved(sample_rate: u32, channels: usize, data: &[f32]) -> Self {
        let channels = channels.max(1);
        let n = data.len() / channels;
        let mut out = Self::new(sample_rate, channels, n);
        for (i, frame) in data.chunks_exact(channels).enumerate() {
            for (c, s) in frame.iter().enumerate() {
                out.channels[c][i] = *s;
            }
        }
        out
    }

    /// Append `other` (same channel count; a buffer with no channels takes `other`'s layout).
    pub fn append(&mut self, other: &AudioBuffer) {
        if self.channels.is_empty() {
            self.channels = vec![Vec::new(); other.channels.len()];
            self.sample_rate = other.sample_rate;
        }
        for (d, s) in self.channels.iter_mut().zip(&other.channels) {
            d.extend_from_slice(s);
        }
    }

    /// Drop the first `n` frames (encoder delay / pre-skip).
    pub fn skip_front(&mut self, n: usize) {
        for c in &mut self.channels {
            let n = n.min(c.len());
            c.drain(..n);
        }
    }

    /// Keep at most `n` frames.
    pub fn truncate(&mut self, n: usize) {
        for c in &mut self.channels {
            c.truncate(n);
        }
    }
}

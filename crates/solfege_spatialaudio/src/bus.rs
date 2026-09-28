//! The multichannel mix bus every spatial source adds into.

/// A planar buffer of `channels` × `capacity` samples.
///
/// Sized once; `clear` and the accessors never allocate. Reads and writes are
/// clamped to the capacity rather than panicking, so a caller that passes a
/// longer block than it allocated for loses the excess instead of the audio
/// thread.
#[derive(Debug, Clone)]
pub struct SpatialBus {
    channels: usize,
    capacity: usize,
    data: Vec<f32>,
}

impl SpatialBus {
    pub fn new(channels: usize, capacity: usize) -> Self {
        Self {
            channels,
            capacity,
            data: vec![0.0; channels * capacity],
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Zero the first `frames` of every channel.
    pub fn clear(&mut self, frames: usize) {
        let frames = frames.min(self.capacity);
        for channel in 0..self.channels {
            let start = channel * self.capacity;
            self.data[start..start + frames].fill(0.0);
        }
    }

    pub fn channel(&self, channel: usize, frames: usize) -> &[f32] {
        if channel >= self.channels {
            return &[];
        }
        let start = channel * self.capacity;
        &self.data[start..start + frames.min(self.capacity)]
    }

    pub fn channel_mut(&mut self, channel: usize, frames: usize) -> &mut [f32] {
        if channel >= self.channels {
            return &mut [];
        }
        let start = channel * self.capacity;
        &mut self.data[start..start + frames.min(self.capacity)]
    }

    /// Two distinct channels at once, mutably.
    pub fn pair_mut(&mut self, a: usize, b: usize, frames: usize) -> (&mut [f32], &mut [f32]) {
        let frames = frames.min(self.capacity);
        if a == b || a >= self.channels || b >= self.channels {
            return (&mut [], &mut []);
        }
        let (lo, hi, swap) = if a < b { (a, b, false) } else { (b, a, true) };
        let (head, tail) = self.data.split_at_mut(hi * self.capacity);
        let first = &mut head[lo * self.capacity..lo * self.capacity + frames];
        let second = &mut tail[..frames];
        if swap {
            (second, first)
        } else {
            (first, second)
        }
    }

    /// Add `gain × input` into `channel`.
    pub fn add(&mut self, channel: usize, input: &[f32], gain: f32) {
        let out = self.channel_mut(channel, input.len());
        for (o, i) in out.iter_mut().zip(input) {
            *o += i * gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_are_independent_and_clear_zeroes_them() {
        let mut bus = SpatialBus::new(3, 4);
        bus.add(1, &[1.0, 2.0, 3.0, 4.0], 0.5);
        assert_eq!(bus.channel(1, 4), &[0.5, 1.0, 1.5, 2.0]);
        assert_eq!(bus.channel(0, 4), &[0.0; 4]);
        let (a, b) = bus.pair_mut(2, 0, 2);
        a[0] = 7.0;
        b[1] = 9.0;
        assert_eq!(bus.channel(2, 1), &[7.0]);
        assert_eq!(bus.channel(0, 2), &[0.0, 9.0]);
        bus.clear(4);
        assert!((0..3).all(|c| bus.channel(c, 4).iter().all(|s| *s == 0.0)));
    }

    #[test]
    fn out_of_range_is_empty_not_a_panic() {
        let mut bus = SpatialBus::new(2, 4);
        assert!(bus.channel(5, 4).is_empty());
        assert_eq!(bus.channel(0, 99).len(), 4);
        let (a, b) = bus.pair_mut(1, 1, 4);
        assert!(a.is_empty() && b.is_empty());
    }
}

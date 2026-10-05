//! What the editor's pitch graph is told: the newest pitch readings.
//!
//! The plug-in host publishes them through the per-insert level block every
//! built-in has in its shared region ([`SLOTS`] `f32`s wide, the block the
//! Drum Sampler's pads and the Slicer's playheads use). The block is a
//! sliding window, not a stream:
//!
//! * slot 0 — how many readings the DSP has taken, mod 2²⁴ (exact in an
//!   `f32`), counting the newest;
//! * then [`POINTS`] triples, oldest first: the input pitch, the corrected
//!   pitch, and the target note, in MIDI note numbers; [`NONE`] where the
//!   voice was unvoiced or there was no target.
//!
//! The editor polls faster than the window slides past, and stitches
//! windows together by the counter.

/// Width of the published block.
pub const SLOTS: usize = 64;
/// Readings in each block.
pub const POINTS: usize = (SLOTS - 1) / 3;
/// No pitch, or no target.
pub const NONE: f32 = -1.0;
/// The counter wraps here.
pub const COUNTER_SPAN: u32 = 1 << 24;

/// One reading, as the graph draws it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// What was sung, in MIDI notes; `None` when unvoiced.
    pub input: Option<f32>,
    /// What comes out.
    pub output: Option<f32>,
    /// The note it is being pulled to.
    pub target: Option<i32>,
}

impl Reading {
    pub const SILENT: Self = Self {
        input: None,
        output: None,
        target: None,
    };
}

/// The newest readings, kept by the DSP. Fixed size: pushing never
/// allocates.
#[derive(Debug, Clone)]
pub struct History {
    readings: [Reading; POINTS],
    head: usize,
    count: u32,
}

impl Default for History {
    fn default() -> Self {
        Self {
            readings: [Reading::SILENT; POINTS],
            head: 0,
            count: 0,
        }
    }
}

impl History {
    pub fn push(&mut self, reading: Reading) {
        self.head = (self.head + 1) % POINTS;
        self.readings[self.head] = reading;
        self.count = (self.count + 1) % COUNTER_SPAN;
    }

    /// The block to publish.
    pub fn encode(&self) -> [f32; SLOTS] {
        let mut slots = [NONE; SLOTS];
        slots[0] = self.count as f32;
        for i in 0..POINTS {
            // Oldest first.
            let reading = self.readings[(self.head + 1 + i) % POINTS];
            let base = 1 + 3 * i;
            slots[base] = reading.input.unwrap_or(NONE);
            slots[base + 1] = reading.output.unwrap_or(NONE);
            slots[base + 2] = reading.target.map_or(NONE, |note| note as f32);
        }
        slots
    }
}

/// Unpacks a block: the counter of its newest reading, and its readings,
/// oldest first.
pub fn decode(slots: &[f32; SLOTS]) -> (u32, [Reading; POINTS]) {
    let pitch = |value: f32| (value.is_finite() && value >= 0.0).then_some(value);
    let count = if slots[0].is_finite() && slots[0] >= 0.0 {
        slots[0] as u32 % COUNTER_SPAN
    } else {
        0
    };
    let readings = std::array::from_fn(|i| {
        let base = 1 + 3 * i;
        Reading {
            input: pitch(slots[base]),
            output: pitch(slots[base + 1]),
            target: pitch(slots[base + 2]).map(|note| note.round() as i32),
        }
    });
    (count, readings)
}

/// How many readings are new in a block whose newest is `count`, after one
/// whose newest was `seen`: at most a window's worth, so a stall shows as a
/// gap, never as readings drawn twice.
pub fn fresh(seen: u32, count: u32) -> usize {
    (count.wrapping_sub(seen) % COUNTER_SPAN).min(POINTS as u32) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_round_trip_oldest_first() {
        let mut history = History::default();
        for i in 0..30 {
            history.push(Reading {
                input: Some(60.0 + i as f32 * 0.01),
                output: (i % 2 == 0).then_some(60.0),
                target: Some(60),
            });
        }
        let (count, readings) = decode(&history.encode());
        assert_eq!(count, 30);
        assert_eq!(readings[POINTS - 1].input, Some(60.29));
        assert_eq!(readings[0].input, Some(60.0 + (30 - POINTS) as f32 * 0.01));
        assert_eq!(readings[POINTS - 1].output, None);
        assert_eq!(fresh(27, 30), 3);
        assert_eq!(fresh(0, 900), POINTS);
        assert_eq!(fresh(COUNTER_SPAN - 2, 1), 3);
    }
}

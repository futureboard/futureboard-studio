//! The voice's centre pitch: where it sits with its vibrato taken out.
//!
//! The note a voice is pulled to, and how far off it is for Flex-Tune, are
//! judged by its centre, not by where its vibrato has swung to this moment:
//! a wide vibrato swings past the half-way point between notes on every
//! cycle, and judged sample by sample it flipped the target and cut the
//! correction in and out at the vibrato's rate.
//!
//! The centre is the middle of the range the pitch has covered over the
//! last [`WINDOW_MS`], which spans a whole cycle of any vibrato a voice
//! sings, so the middle of a vibrato is its centre however wide it is. A
//! leap to another note is told apart from a swing by where it goes: a
//! swing comes back into the range it swings through, while a leap stays
//! clear of everything before it. On a leap the window starts again from
//! the new note, so a melody is followed at once rather than a window late.

/// How far back the range reaches.
const WINDOW_MS: f32 = 250.0;
/// A leap is the newest stretch this long...
const LEAP_RECENT_MS: f32 = 40.0;
/// ...lying wholly this far (semitones) outside the range before it...
const LEAP_MARGIN: f32 = 0.35;
/// ...once the window holds this much before it to compare with.
const LEAP_SETTLED_MS: f32 = 150.0;

/// Pitch readings in a fixed ring, sized at construction.
#[derive(Debug, Clone)]
pub struct Centre {
    ring: Box<[f32]>,
    head: usize,
    len: usize,
    /// The ring's span, in readings: the window, the leap stretch and how
    /// much must come before it.
    window: usize,
    recent: usize,
    settled: usize,
}

impl Centre {
    /// A tracker for readings `hop_seconds` apart.
    pub fn new(hop_seconds: f32) -> Self {
        let readings = |ms: f32| ((ms * 0.001 / hop_seconds.max(1.0e-4)).ceil() as usize).max(1);
        let window = readings(WINDOW_MS).max(2);
        Self {
            ring: vec![0.0; window].into_boxed_slice(),
            head: 0,
            len: 0,
            window,
            recent: readings(LEAP_RECENT_MS).min(window - 1),
            settled: readings(LEAP_SETTLED_MS).min(window - 1),
        }
    }

    /// Forgets the voice: the next reading starts afresh.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Takes one pitch reading (MIDI notes) and returns the centre.
    pub fn push(&mut self, midi: f32) -> f32 {
        self.head = (self.head + 1) % self.window;
        self.ring[self.head] = midi;
        self.len = (self.len + 1).min(self.window);
        if self.leapt() {
            self.len = self.recent;
        }
        let (low, high) = self.range(0, self.len);
        (low + high) * 0.5
    }

    /// The `age`-th newest reading (0 is the newest).
    fn at(&self, age: usize) -> f32 {
        self.ring[(self.head + self.window - age) % self.window]
    }

    /// The lowest and highest readings aged `from..to`.
    fn range(&self, from: usize, to: usize) -> (f32, f32) {
        (from..to).fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), age| {
            let x = self.at(age);
            (low.min(x), high.max(x))
        })
    }

    /// Whether the newest stretch lies wholly clear of everything before it.
    fn leapt(&self) -> bool {
        if self.len < self.recent + self.settled {
            return false;
        }
        let (before_low, before_high) = self.range(self.recent, self.len);
        let (low, high) = self.range(0, self.recent);
        low > before_high + LEAP_MARGIN || high < before_low - LEAP_MARGIN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOP: f32 = 0.004;

    fn run(centre: &mut Centre, seconds: f32, pitch: impl Fn(f32) -> f32) -> Vec<(f32, f32)> {
        (0..(seconds / HOP) as usize)
            .map(|i| {
                let t = i as f32 * HOP;
                (t, centre.push(pitch(t)))
            })
            .collect()
    }

    #[test]
    fn a_wide_vibrato_has_its_middle_for_a_centre() {
        let mut centre = Centre::new(HOP);
        let readings = run(&mut centre, 2.0, |t| {
            69.05 + 0.6 * (std::f32::consts::TAU * 5.5 * t).sin()
        });
        for (t, c) in readings.into_iter().filter(|(t, _)| *t > 0.3) {
            assert!((c - 69.05).abs() < 0.08, "{c} at {t}");
        }
    }

    #[test]
    fn a_leap_moves_the_centre_within_the_leap_stretch() {
        let mut centre = Centre::new(HOP);
        let readings = run(&mut centre, 1.0, |t| {
            let note = if t < 0.5 { 69.0 } else { 71.0 };
            note + 0.3 * (std::f32::consts::TAU * 5.5 * t).sin()
        });
        let after = readings
            .iter()
            .find(|(t, c)| *t >= 0.5 && (c - 71.0).abs() < 0.35)
            .map(|(t, _)| t - 0.5)
            .expect("the centre reached the new note");
        assert!(after < 0.07, "the leap took {after} s to follow");
    }

    #[test]
    fn a_semitone_step_is_a_leap_too() {
        let mut centre = Centre::new(HOP);
        let readings = run(&mut centre, 1.0, |t| if t < 0.5 { 69.0 } else { 70.0 });
        let (_, last) = readings.last().copied().unwrap();
        assert!((last - 70.0).abs() < 1.0e-4);
        let at = readings
            .iter()
            .find(|(t, c)| *t >= 0.5 && (c - 70.0).abs() < 0.01)
            .map(|(t, _)| t - 0.5)
            .unwrap();
        assert!(at < 0.06, "the step took {at} s to follow");
    }
}

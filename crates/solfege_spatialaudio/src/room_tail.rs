//! The room's late tail, for headphones.
//!
//! A source heard with its direct sound and a few early reflections still
//! sits close to the head; the diffuse tail that follows, different at the
//! two ears, is what finishes the job of putting it out in the room. One tail
//! serves the whole mix: past the first reflections a room's sound no longer
//! says where it came from, so every source can share it.
//!
//! An eight-line feedback delay network with a Hadamard mix. The delays grow
//! with the room, the decay time with them, treble dies faster than bass,
//! the input leaves out the low end (a tail on the bass only muddies it), and
//! the two ears read the lines with orthogonal signs, so what they hear is
//! uncorrelated.

use crate::dsp::{DelayLine, PoleZero, PoleZeroState};
use crate::position::RoomSettings;

const LINES: usize = 8;
/// Line lengths for a room of 3 m half-size, milliseconds: mutually prime-ish
/// so their echoes never line up.
const BASE_DELAYS_MS: [f32; LINES] = [29.7, 37.1, 41.1, 43.7, 53.9, 59.3, 67.1, 73.7];
/// The tail's level at full room amount, against the direct sound.
const TAIL_LEVEL: f32 = 0.5;
/// Below this the tail takes nothing in.
const INPUT_HIGHPASS_HZ: f32 = 180.0;
/// Treble decays through this per pass around a line.
const DAMPING_HZ: f32 = 6_500.0;

/// Signs of each ear's read of the lines: two rows of a Hadamard matrix.
const EAR_L: [f32; LINES] = [1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0];
const EAR_R: [f32; LINES] = [1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
/// Signs the input enters the lines with.
const INPUT: [f32; LINES] = [1.0, -1.0, -1.0, 1.0, 1.0, 1.0, -1.0, -1.0];

/// The late tail of the room a headphone mix is heard in.
#[derive(Debug, Clone)]
pub struct RoomTail {
    lines: [DelayLine; LINES],
    delays: [f32; LINES],
    feedback: [f32; LINES],
    damping: PoleZero,
    damp_state: [PoleZeroState; LINES],
    highpass: PoleZero,
    highpass_state: PoleZeroState,
    input_gain: f32,
    output_gain: f32,
}

impl RoomTail {
    /// A tail for `room` at `sample_rate`. Allocates the lines.
    pub fn new(room: &RoomSettings, sample_rate: u32) -> Self {
        let room = room.sanitized();
        let rate = sample_rate.max(8_000) as f32;
        // Bigger rooms: longer paths between reflections and a longer decay.
        let scale = (room.half_size_m / 3.0).powf(0.7);
        let rt60 = 0.25 + 0.06 * room.half_size_m;
        let mut delays = [0.0f32; LINES];
        let mut feedback = [0.0f32; LINES];
        for i in 0..LINES {
            delays[i] = (BASE_DELAYS_MS[i] * scale * 0.001 * rate).max(8.0);
            // -60 dB after rt60 seconds, a pass at a time.
            feedback[i] = 10f32.powf(-3.0 * delays[i] / (rt60 * rate));
        }
        let mean_g2 = feedback.iter().map(|g| g * g).sum::<f32>() / LINES as f32;
        Self {
            lines: std::array::from_fn(|i| DelayLine::new(delays[i].ceil() as usize + 4)),
            delays,
            feedback,
            damping: PoleZero::lowpass(DAMPING_HZ, rate),
            damp_state: Default::default(),
            highpass: PoleZero::lowpass(INPUT_HIGHPASS_HZ, rate),
            highpass_state: PoleZeroState::default(),
            // What the loop adds up to over its decay, taken back out, so
            // the tail's level follows the room amount rather than its size.
            input_gain: (1.0 - mean_g2).max(1.0e-4).sqrt() / (LINES as f32).sqrt(),
            output_gain: TAIL_LEVEL * room.reflections / (LINES as f32 / 2.0).sqrt(),
        }
    }

    /// Whether the tail makes any sound at all.
    pub fn is_audible(&self) -> bool {
        self.output_gain > 0.0
    }

    /// Feed the mix's ears in and add the tail to them.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        if !self.is_audible() {
            return;
        }
        let frames = left.len().min(right.len());
        for n in 0..frames {
            let mono = 0.5 * (left[n] + right[n]);
            // Only above the low end.
            let input = (mono - self.highpass_state.tick(self.highpass, mono)) * self.input_gain;

            let mut taps = [0.0f32; LINES];
            for i in 0..LINES {
                let y = self.lines[i].read_linear(self.delays[i]);
                taps[i] = self.damp_state[i].tick(self.damping, y);
            }
            let mixed = hadamard8(taps);
            for i in 0..LINES {
                self.lines[i].push(INPUT[i] * input + self.feedback[i] * mixed[i]);
            }

            let mut l = 0.0;
            let mut r = 0.0;
            for i in 0..LINES {
                l += EAR_L[i] * taps[i];
                r += EAR_R[i] * taps[i];
            }
            left[n] += l * self.output_gain;
            right[n] += r * self.output_gain;
        }
    }

    pub fn reset(&mut self) {
        for line in &mut self.lines {
            line.reset();
        }
        for state in &mut self.damp_state {
            state.reset();
        }
        self.highpass_state.reset();
    }
}

/// The orthonormal 8-point Hadamard transform: every line feeds every other,
/// energy kept.
#[inline]
fn hadamard8(mut x: [f32; LINES]) -> [f32; LINES] {
    let mut h = 1;
    while h < LINES {
        for i in (0..LINES).step_by(h * 2) {
            for j in i..i + h {
                let (a, b) = (x[j], x[j + h]);
                x[j] = a + b;
                x[j + h] = a - b;
            }
        }
        h *= 2;
    }
    let norm = 1.0 / (LINES as f32).sqrt();
    x.map(|v| v * norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn impulse_response(room: RoomSettings, seconds: f32) -> (Vec<f32>, Vec<f32>) {
        let rate = 48_000;
        let mut tail = RoomTail::new(&room, rate);
        let n = (seconds * rate as f32) as usize;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        tail.process(&mut l, &mut r);
        l[0] = 0.0;
        r[0] = 0.0;
        (l, r)
    }

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|s| s * s).sum()
    }

    #[test]
    fn no_reflections_no_tail() {
        let (l, r) = impulse_response(
            RoomSettings {
                reflections: 0.0,
                ..RoomSettings::default()
            },
            0.5,
        );
        assert!(l.iter().chain(&r).all(|s| *s == 0.0));
    }

    #[test]
    fn the_tail_decays_and_the_ears_differ() {
        let (l, r) = impulse_response(RoomSettings::default(), 2.0);
        assert!(l.iter().chain(&r).all(|s| s.is_finite()));
        let early = energy(&l[..24_000]);
        let late = energy(&l[72_000..]);
        assert!(
            early > 0.0 && late < early * 1.0e-3,
            "early {early} late {late}"
        );
        // Uncorrelated ears: the diffuse field a real room leaves.
        let cross: f32 = l.iter().zip(&r).map(|(a, b)| a * b).sum();
        let corr = cross / (energy(&l) * energy(&r)).sqrt();
        assert!(corr.abs() < 0.3, "correlation {corr}");
    }

    #[test]
    fn a_bigger_room_rings_longer() {
        let tail_energy = |half_size_m: f32| {
            let (l, _) = impulse_response(
                RoomSettings {
                    half_size_m,
                    ..RoomSettings::default()
                },
                2.0,
            );
            energy(&l[36_000..]) / energy(&l)
        };
        assert!(tail_energy(10.0) > tail_energy(2.0) * 4.0);
    }
}

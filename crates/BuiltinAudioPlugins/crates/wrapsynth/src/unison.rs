//! WrapSynth's unison: every voice of a stack is its own oscillator, with
//! its own phase, pitch, place in the stereo field and a little drift.
//!
//! A stack's shape comes from the patch alone, so it is worked out once per
//! edit ([`UnisonLayout`]), off the per-sample path: where each voice sits in
//! pitch (a curve, so the stack never realigns on a fixed beat the way
//! evenly spaced voices do), where it sits in the stereo field (both sides
//! of every pair get one sharp and one flat voice, so neither channel is
//! out of tune on its own), and how loud it is (the centre against the
//! outer voices, then the whole stack scaled to the same energy whatever
//! its size).
//!
//! What varies from note to note lives in each voice's [`UnisonOscillator`]:
//! where its cycle started, and — with Analog up — a slow, smoothed pitch
//! drift of a cent or two, a fraction of a decibel and a touch of pan.

use crate::MAX_UNISON;

/// The most a voice's pitch wanders, either way, with Analog at full.
pub const ANALOG_MAX_CENTS: f32 = 2.0;
/// The most a voice's level differs from note to note, with Analog at full:
/// ±5 %, under half a decibel.
const ANALOG_GAIN: f32 = 0.05;
/// The most a voice's pan differs from note to note, with Analog at full.
const ANALOG_PAN: f32 = 0.06;
/// The most a voice's start is nudged round its cycle, with Analog at full.
const ANALOG_PHASE: f32 = 0.02;
/// How often each voice picks a new pitch to drift toward.
const DRIFT_STEP_SEC: f32 = 0.15;
/// How long a voice takes to drift most of the way there.
const DRIFT_SMOOTH_SEC: f32 = 0.25;
/// The golden ratio's fraction: deterministic start phases that never line
/// up, however many voices there are.
const PHASE_STEP: f32 = 0.618_034;
/// How far the shape control bends the detune curve: 2^±1.5.
const SHAPE_RANGE: f32 = 1.5;

/// One stack's shape: every voice's pitch, pan and level, from the voice
/// count, shape and blend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnisonLayout {
    pub count: usize,
    /// −1..1: each voice's share of the detune, lowest first.
    pub detune: [f32; MAX_UNISON],
    /// −1..1: each voice's share of the spread.
    pub pan: [f32; MAX_UNISON],
    /// Each voice's level, the stack scaled to unit energy.
    pub weight: [f32; MAX_UNISON],
}

/// The exponent `shape` (−1..1) puts on the detune curve: above 1 bunches
/// the voices toward the centre pitch, below 1 toward the outside.
pub fn detune_curve(shape: f32) -> f32 {
    2.0f32.powf(shape.clamp(-1.0, 1.0) * SHAPE_RANGE)
}

impl UnisonLayout {
    pub const SINGLE: Self = {
        let mut weight = [0.0; MAX_UNISON];
        weight[0] = 1.0;
        Self {
            count: 1,
            detune: [0.0; MAX_UNISON],
            pan: [0.0; MAX_UNISON],
            weight,
        }
    };

    /// `count` voices (1..=[`MAX_UNISON`]); `shape` (−1..1) curves their
    /// detune; `blend` (0..1) is the outer voices against the centre one
    /// (or, for an even count, the innermost pair): 0 the centre alone, ½
    /// all alike, 1 the outer voices alone.
    pub fn new(count: u8, shape: f32, blend: f32) -> Self {
        let count = usize::from(count).clamp(1, MAX_UNISON);
        if count == 1 {
            return Self::SINGLE;
        }
        let curve = detune_curve(shape);
        let blend = blend.clamp(0.0, 1.0);
        let (centre_level, outer_level) = if count == 2 {
            // One pair: there is nothing to blend.
            (1.0, 1.0)
        } else {
            ((2.0 * (1.0 - blend)).min(1.0), (2.0 * blend).min(1.0))
        };
        let pairs = count / 2;
        let mut layout = Self {
            count,
            detune: [0.0; MAX_UNISON],
            pan: [0.0; MAX_UNISON],
            weight: [0.0; MAX_UNISON],
        };
        // Exactly symmetric: the numerator is a whole number either side of
        // zero.
        let linear = |i: usize| (2 * i as i32 - (count as i32 - 1)) as f32 / (count - 1) as f32;
        // Pairs, from the outside in, spread evenly across the field: each
        // pair's flat voice goes to whichever side is sharper so far, so
        // both channels stay as near the note as the pairs allow. The
        // outermost pair's flat voice goes left.
        let mut sharp_right = 0.0;
        for pair in 0..pairs {
            let width = linear(pair).abs();
            let lean = 2.0 * width * linear(pair).abs().powf(curve);
            let side = if sharp_right > 0.0 { 1.0 } else { -1.0 };
            sharp_right -= side * lean;
            layout.pan[pair] = side * width;
            layout.pan[count - 1 - pair] = -side * width;
        }
        let mut energy = 0.0;
        for i in 0..count {
            let position = linear(i);
            layout.detune[i] = position.signum() * position.abs().powf(curve);
            let pair = i.min(count - 1 - i);
            let centre = if count % 2 == 1 {
                pair == pairs
            } else {
                pair + 1 == pairs
            };
            let level = if centre { centre_level } else { outer_level };
            layout.weight[i] = level;
            energy += level * level;
        }
        // Voices at different pitches add in power, not amplitude: scaling
        // by the root of the summed energy keeps a stack as loud as a single
        // voice however many it has, where dividing by the count thinned it.
        let scale = if energy > 0.0 {
            energy.sqrt().recip()
        } else {
            0.0
        };
        for weight in &mut layout.weight[..count] {
            *weight *= scale;
        }
        layout
    }
}

/// One voice of one oscillator's stack, in one played note.
#[derive(Debug, Clone, Copy)]
pub struct UnisonOscillator {
    /// 0..1 of the cycle.
    pub phase: f64,
    /// Where the pitch has drifted to, in cents.
    pub drift_cents: f32,
    /// Where it is drifting toward.
    pub drift_target: f32,
    /// This note's level for this voice, about 1.
    pub gain: f32,
    /// This note's nudge to this voice's pan.
    pub pan_offset: f32,
}

impl UnisonOscillator {
    pub const ZERO: Self = Self {
        phase: 0.0,
        drift_cents: 0.0,
        drift_target: 0.0,
        gain: 1.0,
        pan_offset: 0.0,
    };

    /// The voice starting a note. `index` is its place in the stack;
    /// `start` (0..1) where the oscillator's phase control puts the cycle;
    /// `random` (0..1) how much of the start is left to chance; `analog`
    /// (0..1) how unlike the last note it may be. `uniform` gives a fresh
    /// number in 0..1 each call.
    pub fn start(
        index: usize,
        start: f32,
        random: f32,
        analog: f32,
        mut uniform: impl FnMut() -> f32,
    ) -> Self {
        let mut bipolar = || uniform() * 2.0 - 1.0;
        let spread = index as f32 * PHASE_STEP;
        let chance = random * bipolar().abs();
        let nudge = analog * ANALOG_PHASE * bipolar();
        let reach = analog * ANALOG_MAX_CENTS;
        let drift = reach * bipolar();
        Self {
            phase: f64::from(start + spread + chance + nudge).rem_euclid(1.0),
            drift_cents: drift,
            drift_target: drift,
            gain: 1.0 + analog * ANALOG_GAIN * bipolar(),
            pan_offset: analog * ANALOG_PAN * bipolar(),
        }
    }

    /// One sample further on at `increment` cycles per sample: the phase to
    /// read at.
    #[inline]
    pub fn advance(&mut self, increment: f64) -> f32 {
        let mut phase = self.phase + increment;
        if phase >= 1.0 {
            phase -= phase.floor();
        }
        self.phase = phase;
        phase as f32
    }

    /// One sample closer to where the drift is heading.
    #[inline]
    pub fn drift(&mut self, smoothing: f32) {
        self.drift_cents += (self.drift_target - self.drift_cents) * smoothing;
    }
}

/// The drift's timing at one sample rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drift {
    /// Samples between new targets.
    pub step: u32,
    /// The per-sample share of the way to the target.
    pub smoothing: f32,
}

impl Drift {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = sample_rate.max(1.0);
        Self {
            step: (DRIFT_STEP_SEC * sample_rate).max(1.0) as u32,
            smoothing: 1.0 - (-1.0 / (DRIFT_SMOOTH_SEC * sample_rate)).exp(),
        }
    }
}

/// `2^(cents / 1200)` for the few tens of cents unison and drift move a
/// voice: a cubic, within a thousandth of a cent up to ±100 cents, where
/// `powf` per voice per sample would cost more than the oscillator.
#[inline]
pub fn cents_ratio_fast(cents: f32) -> f32 {
    let y = cents * (std::f32::consts::LN_2 / 1_200.0);
    1.0 + y * (1.0 + y * (0.5 + y * (1.0 / 6.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seven_voices_sit_on_a_curve_with_a_centre() {
        let layout = UnisonLayout::new(7, 0.07, 0.5);
        assert_eq!(layout.count, 7);
        assert_eq!(layout.detune[3], 0.0, "one voice stays in the centre");
        assert_eq!(layout.pan[3], 0.0);
        assert_eq!((layout.detune[0], layout.detune[6]), (-1.0, 1.0));
        // Symmetric, rising, and a little bunched toward the centre: about
        // ±0.30, ±0.65, ±1.
        for i in 0..3 {
            assert_eq!(layout.detune[i], -layout.detune[6 - i]);
            assert!(layout.detune[i] < layout.detune[i + 1]);
        }
        assert!(
            (layout.detune[4] - 0.30).abs() < 0.02,
            "{:?}",
            layout.detune
        );
        assert!(
            (layout.detune[5] - 0.65).abs() < 0.02,
            "{:?}",
            layout.detune
        );
        // The outer pair hard left and right, the others in between.
        let mut pans: Vec<f32> = layout.pan[..7].to_vec();
        pans.sort_by(f32::total_cmp);
        let expected = [-1.0, -2.0 / 3.0, -1.0 / 3.0, 0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0];
        for (pan, expected) in pans.iter().zip(expected) {
            assert!((pan - expected).abs() < 1.0e-6, "{pans:?}");
        }
    }

    #[test]
    fn neither_side_is_out_of_tune_on_its_own() {
        // How far the sharp voices lean right: the left channel's pitch,
        // weighted by how much of each voice it gets, is minus half this.
        let lean = |layout: &UnisonLayout, pans: &[f32]| -> f32 {
            (0..layout.count).map(|i| layout.detune[i] * pans[i]).sum()
        };
        for count in 3..=MAX_UNISON as u8 {
            let layout = UnisonLayout::new(count, 0.07, 0.5);
            // Panned in pitch order, lowest hard left: one side all flat.
            let in_order: Vec<f32> = (0..layout.count)
                .map(|i| i as f32 / (layout.count - 1) as f32 * 2.0 - 1.0)
                .collect();
            let balanced = lean(&layout, &layout.pan).abs();
            let ordered = lean(&layout, &in_order).abs();
            // One pair can only go one way round.
            assert!(
                balanced <= ordered + 1.0e-6,
                "{count}: {balanced} vs {ordered}"
            );
            if count >= 4 {
                assert!(balanced < ordered, "{count}: {balanced} vs {ordered}");
            }
            if count >= 6 {
                assert!(balanced < ordered * 0.5, "{count}: {balanced} vs {ordered}");
            }
            // The same places in the field, just shared out differently.
            let mut sorted = layout.pan[..layout.count].to_vec();
            sorted.sort_by(f32::total_cmp);
            for (pan, expected) in sorted.iter().zip(&in_order) {
                assert!((pan - expected).abs() < 1.0e-6, "{count}: {sorted:?}");
            }
        }
        // The lowest voice goes left, the highest right.
        let layout = UnisonLayout::new(7, 0.0, 0.5);
        assert_eq!((layout.pan[0], layout.pan[6]), (-1.0, 1.0));
    }

    #[test]
    fn every_stack_carries_the_same_energy() {
        for count in 1..=MAX_UNISON as u8 {
            for blend in [0.0, 0.3, 0.5, 1.0] {
                let layout = UnisonLayout::new(count, 0.3, blend);
                let energy: f32 = layout.weight[..layout.count].iter().map(|w| w * w).sum();
                assert!(
                    (energy - 1.0).abs() < 1.0e-5,
                    "{count} at {blend}: {energy}"
                );
            }
        }
        // The blend moves level between the centre and the outer voices.
        let centred = UnisonLayout::new(7, 0.0, 0.2);
        assert!(centred.weight[3] > centred.weight[0]);
        let outer = UnisonLayout::new(7, 0.0, 0.8);
        assert!(outer.weight[3] < outer.weight[0]);
        assert_eq!(UnisonLayout::new(7, 0.0, 0.0).weight[0], 0.0);
        let even = UnisonLayout::new(2, 0.0, 1.0);
        assert!(even.weight[0] > 0.7 && even.weight[1] > 0.7);
    }

    #[test]
    fn the_shape_bunches_voices_in_or_out() {
        let centre = UnisonLayout::new(5, 1.0, 0.5);
        let linear = UnisonLayout::new(5, 0.0, 0.5);
        let outer = UnisonLayout::new(5, -1.0, 0.5);
        assert!((linear.detune[3] - 0.5).abs() < 1.0e-6);
        assert!(centre.detune[3] < linear.detune[3]);
        assert!(outer.detune[3] > linear.detune[3]);
    }

    #[test]
    fn the_fast_cents_ratio_matches_powf() {
        for cents in [-100.0f32, -50.0, -7.3, 0.0, 0.4, 25.0, 100.0] {
            let exact = 2.0f32.powf(cents / 1_200.0);
            assert!((cents_ratio_fast(cents) - exact).abs() < 1.0e-6, "{cents}");
        }
    }

    #[test]
    fn a_stable_voice_never_drifts_and_an_analog_one_stays_close() {
        let mut seed = 0x1234_5678u32;
        let mut uniform = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32
        };
        let digital = UnisonOscillator::start(2, 0.25, 0.0, 0.0, &mut uniform);
        assert_eq!(digital.drift_cents, 0.0);
        assert_eq!((digital.gain, digital.pan_offset), (1.0, 0.0));
        let analog = UnisonOscillator::start(2, 0.25, 0.0, 1.0, &mut uniform);
        assert!(analog.drift_cents.abs() <= ANALOG_MAX_CENTS);
        assert!((analog.gain - 1.0).abs() <= ANALOG_GAIN);
        // Without randomness, voices start apart — never all on one phase.
        let starts: Vec<f64> = (0..7)
            .map(|i| UnisonOscillator::start(i, 0.0, 0.0, 0.0, &mut uniform).phase)
            .collect();
        for (i, a) in starts.iter().enumerate() {
            for b in &starts[i + 1..] {
                assert!((a - b).abs() > 0.02, "{starts:?}");
            }
        }
    }
}

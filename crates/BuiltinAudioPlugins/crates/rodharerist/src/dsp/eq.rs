//! Studio parametric EQ stage — four fixed-role bands (low shelf, two
//! sweepable bells, high shelf) on the crate's [`StereoBiquad`], same
//! cascade pattern as the cabinet sim. Flat settings are bit-transparent by
//! construction: a band at 0 dB installs no filter at all.
//!
//! Three models share this one architecture and the same six knobs (the
//! sweepable bells keep their user-set frequencies regardless of model) —
//! only the fixed shelf corners and bell Q, i.e. the character of the
//! curve, change per model. This is the same "one engine, tuned profiles"
//! shape as the amp/cab/drive stages.

use builtin_dsp_core::make_eq_coefficients;

use super::{EqModel, StereoBiquad};

const SHELF_Q: f32 = 0.707;

#[derive(Debug, Clone, Copy)]
struct EqProfile {
    low_shelf_hz: f32,
    high_shelf_hz: f32,
    bell_q: f32,
}

impl EqProfile {
    fn for_model(model: EqModel) -> Self {
        match model {
            // The original curve: unchanged so existing projects/presets
            // sound exactly as they always have.
            EqModel::Studio => Self {
                low_shelf_hz: 120.0,
                high_shelf_hz: 6_000.0,
                bell_q: 0.9,
            },
            // Passive-console character: a lower shelf corner (broader,
            // "bigger" low control) and an earlier top roll-off point, with
            // wide, smooth bells instead of surgical notches.
            EqModel::Vintage => Self {
                low_shelf_hz: 90.0,
                high_shelf_hz: 4_500.0,
                bell_q: 0.7,
            },
            // Surgical/digital character: extended shelf range and narrow
            // bells for precise, small-area cuts and boosts.
            EqModel::Modern => Self {
                low_shelf_hz: 150.0,
                high_shelf_hz: 8_000.0,
                bell_q: 1.4,
            },
            EqModel::Air => Self {
                low_shelf_hz: 70.0,
                high_shelf_hz: 12_000.0,
                bell_q: 0.5,
            },
            EqModel::BassShelf => Self {
                low_shelf_hz: 45.0,
                high_shelf_hz: 5_000.0,
                bell_q: 0.75,
            },
            EqModel::Presence => Self {
                low_shelf_hz: 200.0,
                high_shelf_hz: 2_400.0,
                bell_q: 1.1,
            },
            EqModel::Notch => Self {
                low_shelf_hz: 140.0,
                high_shelf_hz: 7_000.0,
                bell_q: 2.8,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct EqStage {
    sample_rate: f32,
    low: StereoBiquad,
    mid1: StereoBiquad,
    mid2: StereoBiquad,
    high: StereoBiquad,
}

impl EqStage {
    pub(super) fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: sample_rate.max(1.0),
            low: StereoBiquad::none(),
            mid1: StereoBiquad::none(),
            mid2: StereoBiquad::none(),
            high: StereoBiquad::none(),
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
    }

    pub(super) fn reset(&mut self) {
        self.low.reset();
        self.mid1.reset();
        self.mid2.reset();
        self.high.reset();
    }

    /// Editor units: gains ±15 dB, `mid1_freq` 100..1000 Hz,
    /// `mid2_freq` 600..6000 Hz.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure(
        &mut self,
        model: EqModel,
        low_gain: f32,
        mid1_freq: f32,
        mid1_gain: f32,
        mid2_freq: f32,
        mid2_gain: f32,
        high_gain: f32,
    ) {
        let sr = self.sample_rate;
        let profile = EqProfile::for_model(model);
        // A 0 dB band is a true bypass (no biquad), so a flat EQ adds zero
        // filter state and is exactly transparent.
        let band = |kind: &str, freq: f32, gain: f32, q: f32| {
            if gain.abs() < 0.05 {
                None
            } else {
                make_eq_coefficients(kind, freq, gain.clamp(-15.0, 15.0), q, sr)
            }
        };
        self.low
            .set(band("lowshelf", profile.low_shelf_hz, low_gain, SHELF_Q));
        self.mid1.set(band(
            "bell",
            mid1_freq.clamp(100.0, 1_000.0),
            mid1_gain,
            profile.bell_q,
        ));
        self.mid2.set(band(
            "bell",
            mid2_freq.clamp(600.0, 6_000.0),
            mid2_gain,
            profile.bell_q,
        ));
        self.high
            .set(band("highshelf", profile.high_shelf_hz, high_gain, SHELF_Q));
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let (mut l, mut r) = self.low.run(left, right);
        (l, r) = self.mid1.run(l, r);
        (l, r) = self.mid2.run(l, r);
        self.high.run(l, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_eq_is_bit_transparent() {
        for &model in EqModel::ALL {
            let mut eq = EqStage::new(48_000.0);
            eq.configure(model, 0.0, 400.0, 0.0, 2_000.0, 0.0, 0.0);
            for n in 0..1_000 {
                let x = (n as f32 * 0.05).sin() * 0.7;
                assert_eq!(eq.process(x, -x), (x, -x), "{model:?}");
            }
        }
    }

    #[test]
    fn boosts_change_the_signal_and_stay_finite() {
        let mut eq = EqStage::new(48_000.0);
        eq.configure(EqModel::Studio, 6.0, 400.0, -4.0, 2_000.0, 3.0, 5.0);
        let mut differs = false;
        for n in 0..4_000 {
            let x = (n as f32 * 0.05).sin() * 0.5;
            let (l, r) = eq.process(x, x);
            assert!(l.is_finite() && r.is_finite());
            if (l - x).abs() > 1.0e-4 {
                differs = true;
            }
        }
        assert!(differs, "non-flat EQ left the signal untouched");
    }

    /// Same knobs, different model, must sound different — otherwise the
    /// model select is a relabel, not a real voicing change.
    #[test]
    fn models_are_distinct_at_identical_knob_settings() {
        let render = |model: EqModel| {
            let mut eq = EqStage::new(48_000.0);
            eq.configure(model, 6.0, 400.0, -4.0, 2_000.0, 3.0, 5.0);
            (0..2_000)
                .map(|n| eq.process((n as f32 * 0.05).sin() * 0.5, 0.0).0)
                .collect::<Vec<_>>()
        };
        let models = EqModel::ALL;
        let rendered: Vec<_> = models.iter().map(|m| render(*m)).collect();
        for i in 0..rendered.len() {
            for j in (i + 1)..rendered.len() {
                let rms = (rendered[i]
                    .iter()
                    .zip(rendered[j].iter())
                    .map(|(a, b)| (a - b).powi(2))
                    .sum::<f32>()
                    / rendered[i].len() as f32)
                    .sqrt();
                assert!(rms > 1.0e-4, "{:?} == {:?}: {rms}", models[i], models[j]);
            }
        }
    }
}

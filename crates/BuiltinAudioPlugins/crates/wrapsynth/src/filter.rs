//! WrapSynth's filters: three models, each a zero-delay-feedback design
//! from the published virtual-analog literature, so cutoff and resonance can
//! be swept at audio rate without the old Chamberlin filter's high-cutoff
//! instability.
//!
//! * **Analog** — a trapezoidal-integrated state-variable filter (the
//!   Zavalishin / Simper formulation): low-, band-, high-pass and notch, and
//!   a 24 dB low-pass as a flat stage feeding the resonant one.
//! * **Ladder** — four one-pole stages with resolved global feedback
//!   (Zavalishin's ZDF ladder), the feedback path saturated; tapped for
//!   24 and 12 dB low-pass, band- and high-pass.
//! * **Formant** — three band-passes at a vowel's formants, shifted
//!   together by the cutoff.
//!
//! Per voice each channel keeps a [`FilterState`]; the coefficients are
//! worked out once per sample per voice ([`Coefficients`]) and shared by
//! both channels. Nothing here allocates.

use serde::{Deserialize, Serialize};

/// One filter's response. The first four keep the names old projects saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FilterType {
    #[default]
    LowPass12,
    LowPass24,
    HighPass12,
    BandPass12,
    Notch,
    LadderLowPass24,
    LadderLowPass12,
    LadderBandPass,
    LadderHighPass,
    FormantA,
    FormantE,
    FormantI,
    FormantO,
    FormantU,
}

/// The families of [`FilterType`], as the editor groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterModel {
    Analog,
    Ladder,
    Formant,
}

impl FilterModel {
    pub const ALL: [FilterModel; 3] = [Self::Analog, Self::Ladder, Self::Formant];

    pub fn label(self) -> &'static str {
        match self {
            Self::Analog => "Analog",
            Self::Ladder => "Ladder",
            Self::Formant => "Formant",
        }
    }

    pub fn types(self) -> &'static [FilterType] {
        match self {
            Self::Analog => &[
                FilterType::LowPass12,
                FilterType::LowPass24,
                FilterType::BandPass12,
                FilterType::HighPass12,
                FilterType::Notch,
            ],
            Self::Ladder => &[
                FilterType::LadderLowPass24,
                FilterType::LadderLowPass12,
                FilterType::LadderBandPass,
                FilterType::LadderHighPass,
            ],
            Self::Formant => &[
                FilterType::FormantA,
                FilterType::FormantE,
                FilterType::FormantI,
                FilterType::FormantO,
                FilterType::FormantU,
            ],
        }
    }
}

impl FilterType {
    pub const ALL: [FilterType; 14] = [
        Self::LowPass12,
        Self::LowPass24,
        Self::HighPass12,
        Self::BandPass12,
        Self::Notch,
        Self::LadderLowPass24,
        Self::LadderLowPass12,
        Self::LadderBandPass,
        Self::LadderHighPass,
        Self::FormantA,
        Self::FormantE,
        Self::FormantI,
        Self::FormantO,
        Self::FormantU,
    ];

    pub fn to_wire(self) -> f32 {
        self as usize as f32
    }

    pub fn from_wire(value: f32) -> Self {
        Self::ALL[value.round().clamp(0.0, (Self::ALL.len() - 1) as f32) as usize]
    }

    pub fn model(self) -> FilterModel {
        match self {
            Self::LowPass12
            | Self::LowPass24
            | Self::HighPass12
            | Self::BandPass12
            | Self::Notch => FilterModel::Analog,
            Self::LadderLowPass24
            | Self::LadderLowPass12
            | Self::LadderBandPass
            | Self::LadderHighPass => FilterModel::Ladder,
            _ => FilterModel::Formant,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LowPass12 => "LP 12",
            Self::LowPass24 => "LP 24",
            Self::HighPass12 => "HP 12",
            Self::BandPass12 => "BP 12",
            Self::Notch => "Notch",
            Self::LadderLowPass24 => "LP 24",
            Self::LadderLowPass12 => "LP 12",
            Self::LadderBandPass => "BP",
            Self::LadderHighPass => "HP",
            Self::FormantA => "A",
            Self::FormantE => "E",
            Self::FormantI => "I",
            Self::FormantO => "O",
            Self::FormantU => "U",
        }
    }

    fn vowel(self) -> usize {
        match self {
            Self::FormantE => 1,
            Self::FormantI => 2,
            Self::FormantO => 3,
            Self::FormantU => 4,
            _ => 0,
        }
    }
}

/// Formant centres (Hz) and levels per vowel, after Peterson & Barney.
const VOWELS: [[(f32, f32); 3]; 5] = [
    [(730.0, 1.0), (1_090.0, 0.5), (2_440.0, 0.25)],
    [(530.0, 1.0), (1_840.0, 0.45), (2_480.0, 0.3)],
    [(270.0, 1.0), (2_290.0, 0.35), (3_010.0, 0.3)],
    [(570.0, 1.0), (840.0, 0.6), (2_410.0, 0.2)],
    [(300.0, 1.0), (870.0, 0.4), (2_240.0, 0.15)],
];
/// Where the cutoff leaves the formants unshifted.
pub const FORMANT_CENTRE_HZ: f32 = 1_000.0;

/// One channel's memory: two integrators per SVF (up to three SVFs for the
/// formants), or the ladder's four stages.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilterState {
    s: [f32; 6],
}

/// What one sample of one filter needs, shared by both channels.
#[derive(Debug, Clone, Copy)]
pub struct Coefficients {
    kind: FilterType,
    /// Prewarped integrator gains: one, or the three formants'.
    g: [f32; 3],
    /// SVF damping (1/Q), or the ladder's feedback.
    k: f32,
}

#[inline]
fn prewarp(cutoff: f32, sample_rate: f32) -> f32 {
    let nyquist_safe = cutoff.clamp(10.0, sample_rate * 0.49);
    (std::f32::consts::PI * nyquist_safe / sample_rate).tan()
}

impl Coefficients {
    /// `resonance` is 0..1; the analog damping follows the original
    /// WrapSynth curve, so old patches ring the same.
    pub fn new(kind: FilterType, cutoff: f32, resonance: f32, sample_rate: f32) -> Self {
        let resonance = resonance.clamp(0.0, 1.0);
        match kind.model() {
            FilterModel::Analog => Self {
                kind,
                g: [prewarp(cutoff, sample_rate), 0.0, 0.0],
                k: (2.0 - 1.9 * resonance).max(0.08),
            },
            FilterModel::Ladder => Self {
                kind,
                g: [prewarp(cutoff, sample_rate), 0.0, 0.0],
                // Self-oscillation sits at 4.
                k: resonance * 3.96,
            },
            FilterModel::Formant => {
                let shift = (cutoff / FORMANT_CENTRE_HZ).clamp(0.25, 4.0);
                let vowel = &VOWELS[kind.vowel()];
                Self {
                    kind,
                    g: [
                        prewarp(vowel[0].0 * shift, sample_rate),
                        prewarp(vowel[1].0 * shift, sample_rate),
                        prewarp(vowel[2].0 * shift, sample_rate),
                    ],
                    // Narrower with resonance.
                    k: 0.5 - 0.44 * resonance,
                }
            }
        }
    }
}

/// One trapezoidal SVF step on integrators `s[at]`, `s[at + 1]`; returns
/// `(low, band, high)`.
#[inline]
fn svf(s: &mut [f32; 6], at: usize, input: f32, g: f32, k: f32) -> (f32, f32, f32) {
    let a1 = 1.0 / (1.0 + g * (g + k));
    let a2 = g * a1;
    let a3 = g * a2;
    let v3 = input - s[at + 1];
    let v1 = a1 * s[at] + a2 * v3;
    let v2 = s[at + 1] + a2 * s[at] + a3 * v3;
    s[at] = flush(2.0 * v1 - s[at]);
    s[at + 1] = flush(2.0 * v2 - s[at + 1]);
    (v2, v1, input - k * v1 - v2)
}

#[inline]
fn flush(x: f32) -> f32 {
    builtin_dsp_core::flush_denormal(x)
}

/// A soft saturator for the ladder's feedback: tanh's shape, cheaply.
#[inline]
fn saturate(x: f32) -> f32 {
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

impl FilterState {
    pub const ZERO: Self = Self { s: [0.0; 6] };

    pub fn reset(&mut self) {
        self.s = [0.0; 6];
    }

    #[inline]
    pub fn process(&mut self, c: &Coefficients, input: f32) -> f32 {
        let s = &mut self.s;
        match c.kind {
            FilterType::LowPass12 => svf(s, 0, input, c.g[0], c.k).0,
            FilterType::LowPass24 => {
                let flat = svf(s, 0, input, c.g[0], std::f32::consts::SQRT_2).0;
                svf(s, 2, flat, c.g[0], c.k).0
            }
            FilterType::HighPass12 => svf(s, 0, input, c.g[0], c.k).2,
            // Scaled by the damping so the peak stays at unity.
            FilterType::BandPass12 => svf(s, 0, input, c.g[0], c.k).1 * c.k,
            FilterType::Notch => {
                let (low, _, high) = svf(s, 0, input, c.g[0], c.k);
                low + high
            }
            FilterType::LadderLowPass24
            | FilterType::LadderLowPass12
            | FilterType::LadderBandPass
            | FilterType::LadderHighPass => {
                let g = c.g[0];
                let big_g = g / (1.0 + g);
                // Each stage is y = G·x + s/(1+g): what the four would put out
                // with no input, folded back through the feedback.
                let scale = 1.0 / (1.0 + g);
                let sigma = big_g * big_g * big_g * s[0] * scale
                    + big_g * big_g * s[1] * scale
                    + big_g * s[2] * scale
                    + s[3] * scale;
                let g4 = big_g * big_g * big_g * big_g;
                // Bass lost to the feedback, given back at the input.
                let input = input * (1.0 + c.k * 0.5);
                let u = saturate((input - c.k * sigma) / (1.0 + c.k * g4));
                let mut stage_in = u;
                let mut taps = [0.0f32; 4];
                for (stage, tap) in taps.iter_mut().enumerate() {
                    let v = (stage_in - s[stage]) * big_g;
                    let y = v + s[stage];
                    s[stage] = flush(y + v);
                    *tap = y;
                    stage_in = y;
                }
                match c.kind {
                    FilterType::LadderLowPass24 => taps[3],
                    FilterType::LadderLowPass12 => taps[1],
                    FilterType::LadderBandPass => 2.0 * (taps[1] - taps[3]),
                    _ => u - 4.0 * taps[0] + 6.0 * taps[1] - 4.0 * taps[2] + taps[3],
                }
            }
            _ => {
                let vowel = &VOWELS[c.kind.vowel()];
                let mut sum = 0.0;
                for (band, &(_, level)) in vowel.iter().enumerate() {
                    sum += svf(s, band * 2, input, c.g[band], c.k).1 * c.k * level;
                }
                sum * 1.6
            }
        }
    }
}

/// Magnitude in dB at `hz` of the linear prototype of `kind` — what the
/// editor draws. Matches [`FilterState::process`] below saturation.
pub fn response_db(kind: FilterType, cutoff: f32, resonance: f32, hz: f32) -> f32 {
    let resonance = resonance.clamp(0.0, 1.0);
    let x = hz / cutoff.max(1.0);
    // An SVF stage at ratio `x` with damping `k`: low, band, high.
    let stage = |x: f32, k: f32| -> (f32, f32, f32) {
        let d = ((1.0 - x * x).powi(2) + (k * x).powi(2)).sqrt().max(1.0e-9);
        (1.0 / d, k * x / d, x * x / d)
    };
    let magnitude = match kind.model() {
        FilterModel::Analog => {
            let k = (2.0 - 1.9 * resonance).max(0.08);
            let (low, band, high) = stage(x, k);
            match kind {
                FilterType::LowPass24 => low * stage(x, std::f32::consts::SQRT_2).0,
                FilterType::HighPass12 => high,
                FilterType::BandPass12 => band,
                FilterType::Notch => {
                    let d = ((1.0 - x * x).powi(2) + (k * x).powi(2)).sqrt().max(1.0e-9);
                    (1.0 - x * x).abs() / d
                }
                _ => low,
            }
        }
        FilterModel::Ladder => {
            // H = G⁴ / (1 + k·G⁴) with G = 1/(1 + jx): magnitudes through
            // complex arithmetic on (re, im).
            let k = resonance * 3.96;
            let one_pole = (1.0 / (1.0 + x * x), -x / (1.0 + x * x));
            let mul = |a: (f32, f32), b: (f32, f32)| (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0);
            let g2 = mul(one_pole, one_pole);
            let g4 = mul(g2, g2);
            let denominator = (1.0 + k * g4.0, k * g4.1);
            let norm = |a: (f32, f32)| (a.0 * a.0 + a.1 * a.1).sqrt();
            let gain = 1.0 + k * 0.5;
            let tap = match kind {
                FilterType::LadderLowPass12 => norm(g2),
                FilterType::LadderBandPass => 2.0 * norm((g2.0 - g4.0, g2.1 - g4.1)),
                FilterType::LadderHighPass => {
                    let one_minus = (1.0 - one_pole.0, -one_pole.1);
                    let hp2 = mul(one_minus, one_minus);
                    norm(mul(hp2, hp2))
                }
                _ => norm(g4),
            };
            gain * tap / norm(denominator).max(1.0e-9)
        }
        FilterModel::Formant => {
            let shift = (cutoff / FORMANT_CENTRE_HZ).clamp(0.25, 4.0);
            let k = 0.5 - 0.44 * resonance;
            VOWELS[kind.vowel()]
                .iter()
                .map(|&(formant, level)| stage(hz / (formant * shift), k).1 * level)
                .sum::<f32>()
                * 1.6
        }
    };
    20.0 * magnitude.max(1.0e-6).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    /// Steady-state gain of a sine at `hz` through `kind`.
    fn measured_gain(kind: FilterType, cutoff: f32, resonance: f32, hz: f32) -> f32 {
        let c = Coefficients::new(kind, cutoff, resonance, RATE);
        let mut state = FilterState::default();
        let mut peak = 0.0f32;
        for i in 0..24_000 {
            let x = 0.05 * (std::f32::consts::TAU * hz * i as f32 / RATE).sin();
            let y = state.process(&c, x);
            if i > 12_000 {
                peak = peak.max(y.abs());
            }
        }
        peak / 0.05
    }

    #[test]
    fn every_type_passes_its_band_and_stays_finite() {
        for kind in FilterType::ALL {
            let c = Coefficients::new(kind, 1_000.0, 1.0, RATE);
            let mut state = FilterState::default();
            for i in 0..48_000 {
                let x = if i % 97 == 0 { 1.0 } else { 0.0 };
                assert!(state.process(&c, x).is_finite(), "{kind:?}");
            }
        }
        assert!(measured_gain(FilterType::LowPass12, 1_000.0, 0.0, 100.0) > 0.9);
        assert!(measured_gain(FilterType::LowPass24, 1_000.0, 0.0, 8_000.0) < 0.01);
        assert!(measured_gain(FilterType::HighPass12, 1_000.0, 0.0, 100.0) < 0.05);
        assert!(measured_gain(FilterType::LadderLowPass24, 1_000.0, 0.0, 100.0) > 0.8);
        assert!(measured_gain(FilterType::LadderLowPass24, 1_000.0, 0.0, 8_000.0) < 0.01);
        assert!(measured_gain(FilterType::LadderHighPass, 1_000.0, 0.0, 100.0) < 0.05);
    }

    #[test]
    fn the_drawn_curve_matches_what_plays() {
        for kind in [
            FilterType::LowPass12,
            FilterType::BandPass12,
            FilterType::LadderLowPass24,
            FilterType::LadderLowPass12,
        ] {
            for hz in [200.0, 1_000.0, 3_000.0] {
                let drawn = 10.0f32.powf(response_db(kind, 1_000.0, 0.3, hz) / 20.0);
                let played = measured_gain(kind, 1_000.0, 0.3, hz);
                // The ladder's saturation and the bilinear warp near the
                // cutoff keep this approximate.
                assert!(
                    (drawn - played).abs() < 0.15 * drawn.max(0.2),
                    "{kind:?} at {hz} Hz: drawn {drawn}, played {played}"
                );
            }
        }
    }

    #[test]
    fn models_list_their_own_types() {
        for model in FilterModel::ALL {
            assert!(model.types().iter().all(|kind| kind.model() == model));
        }
        let listed: usize = FilterModel::ALL.iter().map(|m| m.types().len()).sum();
        assert_eq!(listed, FilterType::ALL.len());
        for kind in FilterType::ALL {
            assert_eq!(FilterType::from_wire(kind.to_wire()), kind);
        }
    }
}

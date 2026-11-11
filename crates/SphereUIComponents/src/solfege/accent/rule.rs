//! The deterministic accent rule, and the coefficients it was fitted with.
//!
//! Two jobs. It is the analyser the editor uses when no model is loaded — every
//! Solfege instrument has a rule, only some carry a trained one — and it is the
//! baseline the trained analyser has to beat before it is worth shipping.
//!
//! The coefficients are **fitted, not chosen**. Nine interpretable score
//! features, ridge-regressed against the URMP-derived accent targets on the
//! training split, and copied here verbatim from the training report by
//! `neural/scripts/export_accent.py`. That is the only honest way to document a
//! constant in a file like this: a hand-tuned weight is taste presented as
//! measurement, and there is no ground truth to tune against — which is the
//! entire difficulty this system exists to work around.
//!
//! The features are nine rather than the analyser's thirty-three because each
//! one has to be something a musician would say out loud as a reason a note is
//! emphasised. A coefficient on `is_window_peak` is a claim that can be argued
//! with; a coefficient on the eleventh hidden unit of a GRU is not.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::components::timeline::timeline_state::AccentState;

use super::features::ACCENT_INPUT_FEATURES;

/// Fitted coefficients, as written by the training pipeline.
///
/// Embedded as JSON rather than transcribed into Rust literals so that the
/// numbers in this build are byte-identical to the numbers in the report that
/// justified them, and so a retrain is a file swap rather than an editing pass
/// over forty float literals.
const RULE_JSON: &str = include_str!("rule_coefficients.json");

#[derive(Debug, Deserialize)]
struct RuleFile {
    features: Vec<String>,
    #[serde(default)]
    confidence: f32,
    coefficients: HashMap<String, HashMap<String, f32>>,
    intercepts: HashMap<String, f32>,
    #[serde(default)]
    fitted_on: String,
    #[serde(default)]
    calibration: Option<CalibrationFile>,
}

#[derive(Debug, Deserialize)]
struct CalibrationFile {
    #[serde(default)]
    gains: HashMap<String, f32>,
    #[serde(default)]
    raw_means: HashMap<String, f32>,
    #[serde(default)]
    target_means: HashMap<String, f32>,
}

/// Per-head affine transform that gives the analysis the range of the thing it
/// estimates.
///
/// A minimum-error predictor of a noisy target is always under-dispersed: where
/// the evidence is weak the safest guess is the mean, so predictions bunch.
/// Measured, the rule's prominence has a standard deviation of ~0.05 against a
/// target standard deviation of 0.21 — right about *which* notes are prominent,
/// and saying so in a fifth of the range available. An Accent lane whose bars
/// only move between 0.45 and 0.55 cannot be read and cannot drive an audible
/// difference without a hidden gain elsewhere, which is worse because a hidden
/// gain is a fudge nobody can see.
///
/// This is monotone, so it changes no ordering: which note of a phrase is the
/// most prominent has exactly the same answer before and after. It worsens
/// mean absolute error, and both numbers are in the training report.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpreadCalibration {
    pub gain: [f32; 4],
    pub raw_mean: [f32; 4],
    pub target_mean: [f32; 4],
}

impl SpreadCalibration {
    /// The transform that changes nothing, for a file that carries none.
    fn identity() -> Self {
        Self {
            gain: [1.0; 4],
            raw_mean: [0.0; 4],
            target_mean: [0.0; 4],
        }
    }

    pub fn apply(&self, components: [f32; 4]) -> [f32; 4] {
        let mut out = [0.0_f32; 4];
        for index in 0..4 {
            out[index] = self.target_mean[index]
                + self.gain[index] * (components[index] - self.raw_mean[index]);
        }
        out
    }
}

/// The rule, resolved into index-addressed weights.
#[derive(Debug)]
pub struct AccentRule {
    /// Column of [`ACCENT_INPUT_FEATURES`] each weight applies to.
    columns: Vec<usize>,
    /// `[prominence, attack, agogic, timbre]`, each `columns.len()` long.
    weights: [Vec<f32>; 4],
    intercepts: [f32; 4],
    calibration: SpreadCalibration,
    confidence: f32,
    fitted_on: String,
}

/// Target order. Matches `ACCENT_TARGETS` on the training side.
const TARGETS: [&str; 4] = ["prominence", "attack", "agogic", "timbre"];

static RULE: OnceLock<AccentRule> = OnceLock::new();

/// The fitted rule. Parsed once; panics only if this build shipped a malformed
/// embedded file, which is a build error rather than a runtime condition.
pub fn rule() -> &'static AccentRule {
    RULE.get_or_init(|| {
        let file: RuleFile = serde_json::from_str(RULE_JSON)
            .expect("embedded accent rule coefficients are valid JSON");
        AccentRule::from_file(file).expect("embedded accent rule names known features")
    })
}

impl AccentRule {
    fn from_file(file: RuleFile) -> Option<Self> {
        let columns: Option<Vec<usize>> = file
            .features
            .iter()
            .map(|name| {
                ACCENT_INPUT_FEATURES
                    .iter()
                    .position(|feature| feature == name)
            })
            .collect();
        let columns = columns?;

        let mut weights: [Vec<f32>; 4] = Default::default();
        let mut intercepts = [0.0_f32; 4];
        for (index, target) in TARGETS.iter().enumerate() {
            let row = file.coefficients.get(*target)?;
            weights[index] = file
                .features
                .iter()
                .map(|name| row.get(name).copied().unwrap_or(0.0))
                .collect();
            intercepts[index] = file.intercepts.get(*target).copied().unwrap_or(0.5);
        }
        let calibration = match file.calibration.as_ref() {
            Some(source) => {
                let mut calibration = SpreadCalibration::identity();
                for (index, target) in TARGETS.iter().enumerate() {
                    calibration.gain[index] = source.gains.get(*target).copied().unwrap_or(1.0);
                    calibration.raw_mean[index] =
                        source.raw_means.get(*target).copied().unwrap_or(0.0);
                    calibration.target_mean[index] =
                        source.target_means.get(*target).copied().unwrap_or(0.0);
                }
                calibration
            }
            None => SpreadCalibration::identity(),
        };

        Some(Self {
            columns,
            weights,
            intercepts,
            calibration,
            // A least-squares fit reports one number per note and no spread, so
            // the rule has no uncertainty estimate of its own. It declares a
            // fixed middling confidence rather than borrowing the neural
            // model's calibrated one.
            confidence: if file.confidence > 0.0 {
                file.confidence
            } else {
                0.5
            },
            fitted_on: file.fitted_on,
        })
    }

    /// Which dataset and split these coefficients came from, for the inspector.
    pub fn fitted_on(&self) -> &str {
        &self.fitted_on
    }

    /// The spread match applied to a finished analysis. Public because the
    /// analyser applies it *after* adding the learned correction, not here:
    /// calibrating the rule and then adding an uncalibrated correction would
    /// put the two on different scales.
    pub fn calibration(&self) -> &SpreadCalibration {
        &self.calibration
    }

    /// The four accent components for one note's feature vector.
    ///
    /// Returned unclamped-into-`AccentState` order so the neural path can add a
    /// correction to it before either is clamped; [`Self::accent`] is the
    /// finished value.
    pub fn components(&self, features: &[f32]) -> [f32; 4] {
        let mut out = [0.0_f32; 4];
        for (target, slot) in out.iter_mut().enumerate() {
            let mut sum = self.intercepts[target];
            for (position, &column) in self.columns.iter().enumerate() {
                sum +=
                    self.weights[target][position] * features.get(column).copied().unwrap_or(0.0);
            }
            *slot = sum;
        }
        out
    }

    /// One note's accent, from its feature vector.
    pub fn accent(&self, features: &[f32]) -> AccentState {
        let components = self.calibration.apply(self.components(features));
        AccentState::generated(
            components[0],
            components[1],
            components[2],
            components[3],
            self.confidence,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::MidiNoteState;
    use crate::solfege::accent::features::{
        contexts_from_notes, note_feature_vector, ACCENT_INPUT_SIZE,
    };
    use crate::solfege::accent::meter::Meter;

    #[test]
    fn the_embedded_coefficients_parse_and_name_real_features() {
        let rule = rule();
        assert!(!rule.columns.is_empty());
        assert!(
            !rule.fitted_on.is_empty(),
            "the coefficients must say which data produced them"
        );
    }

    #[test]
    fn a_neutral_feature_vector_produces_an_in_range_accent() {
        let accent = rule().accent(&[0.0_f32; ACCENT_INPUT_SIZE]);
        assert!((0.0..=1.0).contains(&accent.prominence));
        assert!((0.0..=1.0).contains(&accent.attack));
    }

    /// The point of the whole system: identical velocities, different accents.
    #[test]
    fn identical_notes_on_different_beats_get_different_accents() {
        let notes: Vec<MidiNoteState> = (0..8)
            .map(|index| MidiNoteState::new(60, index as f32, 1.0, 96))
            .collect();
        let contexts = contexts_from_notes(&notes, 0.0, 120.0);
        let meter = Meter::from_signature(4, 4);
        let durations: Vec<f32> = contexts.iter().map(|c| c.duration_beats).collect();
        let pitches: Vec<f32> = contexts.iter().map(|c| c.pitch as f32).collect();
        let accents: Vec<AccentState> = (0..contexts.len())
            .map(|index| {
                rule().accent(&note_feature_vector(
                    &contexts, index, &meter, &durations, &pitches,
                ))
            })
            .collect();
        assert!(
            accents[0].prominence > accents[1].prominence,
            "the downbeat must outrank beat 2: {} vs {}",
            accents[0].prominence,
            accents[1].prominence
        );
        assert!(
            accents[2].prominence > accents[1].prominence,
            "beat 3 must outrank beat 2"
        );
    }
}

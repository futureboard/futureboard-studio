//! Offline audio analysis: tempo (BPM), musical key, and instrument/voice
//! classification used to suggest a track name.
//!
//! This is an **offline / control-thread** path. It allocates and runs FFTs
//! over a whole buffer and must never be called from a realtime audio callback.
//! Feed it a decoded clip (e.g. an imported or freshly recorded file) on a
//! background/analysis thread and apply the result via a command outcome.

mod bpm;
pub mod chords;
pub mod chroma;
mod error;
mod features;
mod instrument;
mod key;
pub mod loudness;
pub mod phase;
pub mod rhythm;
pub mod spectrum;
pub mod spectrum_analyzer;
pub mod transients;

#[cfg(feature = "onnx")]
pub mod onnx;

pub use bpm::{
    TempoCandidate, TempoEstimate, TempoFamily, TempoHypothesis, estimate_bpm_candidates,
    octave_ratio, tempo_families,
};
pub use chords::{ChordKind, ChordLabel, ChordOptions, ChordSegment, recognize_chords};
pub use chroma::{ChromaFrames, chroma_frames};
pub use error::AnalysisError;
pub use features::{FEATURE_VECTOR_LEN, SpectralFeatures};
pub use instrument::{Classifier, HeuristicClassifier, InstrumentCategory, InstrumentEstimate};
pub use key::{
    KeyEstimate, KeyMode, PitchClass, estimate_key_ranked, key_correlations, pitch_class_profile,
    rank_keys,
};
pub use loudness::{LoudnessMeasurement, analyze_loudness};
pub use phase::{PhaseMeasurement, measure_phase};
pub use rhythm::{Beat, RhythmAnalysis, RhythmOptions, TempoSection, analyze_rhythm};
pub use spectrum_analyzer::{
    FftSize, SpectrumMode, SpectrumSmoothing, SpectrumSnapshot, SpectrumWindow,
    analyze_ring_window, analyze_spectrum,
};
pub use transients::{FrequencyFocus, TransientDetectParams, TransientMarker, detect_transients};

#[cfg(feature = "onnx")]
pub use onnx::OnnxClassifier;

use serde::{Deserialize, Serialize};

/// Which analyses to run.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnalysisOptions {
    pub detect_bpm: bool,
    pub detect_key: bool,
    pub detect_instrument: bool,
    /// Lower bound for tempo search (BPM).
    pub min_bpm: f32,
    /// Upper bound for tempo search (BPM).
    pub max_bpm: f32,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self {
            detect_bpm: true,
            detect_key: true,
            detect_instrument: true,
            min_bpm: 60.0,
            max_bpm: 200.0,
        }
    }
}

/// Combined analysis result. Every estimate is optional so callers can trust
/// only the fields they asked for and that had enough signal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioAnalysis {
    pub bpm: Option<TempoEstimate>,
    pub key: Option<KeyEstimate>,
    pub instrument: Option<InstrumentEstimate>,
    /// Suggested track name derived from the detected instrument/voice family.
    pub suggested_track_name: Option<String>,
    pub sample_rate: f32,
    pub duration_secs: f32,
}

/// Analyse a mono buffer using the default [`HeuristicClassifier`].
pub fn analyze_mono(samples: &[f32], sample_rate: f32, opts: AnalysisOptions) -> AudioAnalysis {
    analyze_mono_with(samples, sample_rate, opts, &HeuristicClassifier)
}

/// Analyse a mono buffer with a caller-supplied classifier backend.
pub fn analyze_mono_with(
    samples: &[f32],
    sample_rate: f32,
    opts: AnalysisOptions,
    classifier: &dyn Classifier,
) -> AudioAnalysis {
    let duration_secs = if sample_rate > 0.0 {
        samples.len() as f32 / sample_rate
    } else {
        0.0
    };

    let bpm = if opts.detect_bpm {
        bpm::estimate_bpm(samples, sample_rate, opts.min_bpm, opts.max_bpm)
    } else {
        None
    };

    let key = if opts.detect_key {
        key::estimate_key(samples, sample_rate)
    } else {
        None
    };

    let instrument = if opts.detect_instrument {
        features::extract(samples, sample_rate).map(|f| classifier.classify(&f))
    } else {
        None
    };

    let suggested_track_name = instrument.map(|e| e.category.track_name().to_string());

    AudioAnalysis {
        bpm,
        key,
        instrument,
        suggested_track_name,
        sample_rate,
        duration_secs,
    }
}

/// Analyse an interleaved-by-channel stereo pair by downmixing to mono first.
pub fn analyze_stereo(
    left: &[f32],
    right: &[f32],
    sample_rate: f32,
    opts: AnalysisOptions,
) -> AudioAnalysis {
    let mono = downmix(left, right);
    analyze_mono(&mono, sample_rate, opts)
}

fn downmix(left: &[f32], right: &[f32]) -> Vec<f32> {
    let n = left.len().min(right.len());
    let mut mono = Vec::with_capacity(n);
    for i in 0..n {
        mono.push(0.5 * (left[i] + right[i]));
    }
    mono
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn sine(freq: f32, sample_rate: f32, secs: f32) -> Vec<f32> {
        let n = (sample_rate * secs) as usize;
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / sample_rate).sin())
            .collect()
    }

    #[test]
    fn empty_buffer_is_safe() {
        let a = analyze_mono(&[], 48_000.0, AnalysisOptions::default());
        assert!(a.bpm.is_none());
        assert!(a.key.is_none());
        assert!(a.instrument.is_none());
        assert!(a.suggested_track_name.is_none());
    }

    #[test]
    fn key_detects_a_tonic_from_a_major_arpeggio() {
        let sr = 44_100.0;
        // A major triad: A4, C#5, E5 sustained.
        let mut buf = sine(440.0, sr, 3.0);
        for (i, s) in sine(554.37, sr, 3.0).into_iter().enumerate() {
            buf[i] += s;
        }
        for (i, s) in sine(659.25, sr, 3.0).into_iter().enumerate() {
            buf[i] += s;
        }
        let key = key::estimate_key(&buf, sr).expect("key");
        assert_eq!(key.tonic, PitchClass::A);
    }

    #[test]
    fn bass_sine_classifies_low_and_suggests_a_name() {
        let sr = 44_100.0;
        let buf = sine(60.0, sr, 2.0);
        let a = analyze_mono(&buf, sr, AnalysisOptions::default());
        let est = a.instrument.expect("instrument");
        assert!(est.features.low_energy_ratio > 0.5);
        assert_eq!(est.category, InstrumentCategory::Bass);
        assert_eq!(a.suggested_track_name.as_deref(), Some("Bass"));
    }

    #[test]
    fn bright_noise_reads_as_percussive() {
        let sr = 44_100.0;
        // Deterministic pseudo-noise.
        let mut state = 0x1234_5678_u32;
        let buf: Vec<f32> = (0..sr as usize * 2)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
            })
            .collect();
        let f = features::extract(&buf, sr).expect("features");
        assert!(f.flatness > 0.1);
        assert!(f.zero_crossing_rate > 0.1);
    }

    #[test]
    fn analysis_serde_roundtrip() {
        let sr = 44_100.0;
        let buf = sine(220.0, sr, 1.5);
        let a = analyze_mono(&buf, sr, AnalysisOptions::default());
        let encoded = serde_json::to_string(&a).expect("serialize");
        let decoded: AudioAnalysis = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, a);
    }
}

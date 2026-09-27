//! Musical key estimation via chromagram + Krumhansl-Schmuckler key profiles.
//!
//! Offline / control-thread only.

use serde::{Deserialize, Serialize};

use super::spectrum::magnitude_frames;

/// Pitch class (0 = C, 1 = C#/Db, ... 11 = B).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PitchClass {
    C,
    Cs,
    D,
    Ds,
    E,
    F,
    Fs,
    G,
    Gs,
    A,
    As,
    B,
}

impl PitchClass {
    pub fn from_index(index: i32) -> Self {
        match index.rem_euclid(12) {
            0 => Self::C,
            1 => Self::Cs,
            2 => Self::D,
            3 => Self::Ds,
            4 => Self::E,
            5 => Self::F,
            6 => Self::Fs,
            7 => Self::G,
            8 => Self::Gs,
            9 => Self::A,
            10 => Self::As,
            _ => Self::B,
        }
    }

    /// Sharp-spelled name, e.g. `"C#"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::Cs => "C#",
            Self::D => "D",
            Self::Ds => "D#",
            Self::E => "E",
            Self::F => "F",
            Self::Fs => "F#",
            Self::G => "G",
            Self::Gs => "G#",
            Self::A => "A",
            Self::As => "A#",
            Self::B => "B",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyMode {
    Major,
    Minor,
}

impl KeyMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Major => "maj",
            Self::Minor => "min",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Major => "major",
            Self::Minor => "minor",
        }
    }
}

/// Estimated musical key.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyEstimate {
    pub tonic: PitchClass,
    pub mode: KeyMode,
    /// Correlation of the best key vs. the next best, in `[0, 1]`.
    pub confidence: f32,
}

impl KeyEstimate {
    /// Compact label, e.g. `"A min"`.
    pub fn label(&self) -> String {
        format!("{} {}", self.tonic.name(), self.mode.name())
    }

    /// Longer label, e.g. `"A minor"`.
    pub fn display_label(&self) -> String {
        format!("{} {}", self.tonic.name(), self.mode.display_name())
    }
}

// Krumhansl-Kessler tonal hierarchy profiles (major and minor).
const MAJOR_PROFILE: [f32; 12] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const MINOR_PROFILE: [f32; 12] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];

/// Analysis window: long enough that neighbouring semitones in the bass are
/// several bins apart (16 384 samples ≈ 2.7 Hz bins at 44.1 kHz).
const FRAME_SECONDS: f32 = 0.37;
/// Pitched range that carries key information: C2 to about C8. Below it is
/// kick and sub energy whose bin positions, not notes, used to decide the
/// chroma — the old front-end read nearly every track as A# minor / C# major.
const MIN_HZ: f32 = 65.0;
const MAX_HZ: f32 = 4200.0;
/// A peak must stand this far above the local spectral mean to count as a
/// partial. Noise frames are removed by the flatness gate, so this can stay
/// permissive enough to keep the quieter partials of a dense mix.
const PROMINENCE: f32 = 2.0;
/// Half-width, in bins, of the local mean a peak is compared against.
const LOCAL_BINS: usize = 24;
/// Peaks further than this (in semitones) from the tuned grid are inharmonic.
const MAX_DETUNE: f32 = 0.35;
/// How close (semitones) a partial must sit to a lower partial's harmonic to
/// be treated as its overtone, and the share of its vote it then keeps.
const HARMONIC_TOLERANCE: f32 = 0.3;
const HARMONIC_WEIGHT: f32 = 0.4;
/// Frames whose power-spectrum flatness exceeds this are noise, not notes.
/// White noise sits at e^-γ ≈ 0.56; pitched music frames stay under ~0.3.
const MAX_FLATNESS: f32 = 0.4;

/// Estimate the musical key of a mono buffer. Returns `None` if the signal is
/// too short or carries no pitched energy.
pub fn estimate_key(samples: &[f32], sample_rate: f32) -> Option<KeyEstimate> {
    estimate_key_ranked(samples, sample_rate).into_iter().next()
}

/// Ranked key estimates, best first. `detected_key` is index 0; the rest are
/// alternates. User-chosen key is stored separately by the tool window.
pub fn estimate_key_ranked(samples: &[f32], sample_rate: f32) -> Vec<KeyEstimate> {
    match pitch_class_profile(samples, sample_rate) {
        Some(chroma) => rank_keys(&chroma),
        None => Vec::new(),
    }
}

/// One prominent spectral peak: fractional MIDI pitch and weight.
struct Partial {
    frame: u32,
    midi: f32,
    weight: f32,
}

/// Normalized pitch-class energy (index 0 = C, sums to 1) — the chromagram the
/// key estimate is correlated against. `None` for silence or unusable input.
///
/// Built from prominent spectral peaks only, located to a fraction of a bin,
/// corrected for the recording's overall tuning, and normalised per frame so
/// loud sections do not outvote quiet ones.
pub fn pitch_class_profile(samples: &[f32], sample_rate: f32) -> Option<[f32; 12]> {
    if sample_rate <= 0.0 || !sample_rate.is_finite() {
        return None;
    }
    let size = ((sample_rate * FRAME_SECONDS) as usize)
        .next_power_of_two()
        .max(4096);
    let frames = magnitude_frames(samples, size, size / 2);
    if frames.is_empty() {
        return None;
    }

    let partials = collect_partials(&frames, size, sample_rate);
    if partials.is_empty() {
        return None;
    }
    let tuning = estimate_tuning(&partials);

    let mut chroma = [0.0_f32; 12];
    let mut frame_chroma = [0.0_f32; 12];
    let mut current = partials[0].frame;
    let mut flush = |frame_chroma: &mut [f32; 12], chroma: &mut [f32; 12]| {
        let total: f32 = frame_chroma.iter().sum();
        if total > f32::EPSILON {
            for (acc, value) in chroma.iter_mut().zip(frame_chroma.iter()) {
                *acc += value / total;
            }
        }
        *frame_chroma = [0.0; 12];
    };
    for partial in &partials {
        if partial.frame != current {
            flush(&mut frame_chroma, &mut chroma);
            current = partial.frame;
        }
        let tuned = partial.midi - tuning;
        let nearest = tuned.round();
        if (tuned - nearest).abs() > MAX_DETUNE {
            continue;
        }
        let pc = (nearest as i32).rem_euclid(12) as usize;
        frame_chroma[pc] += partial.weight;
    }
    flush(&mut frame_chroma, &mut chroma);

    let total: f32 = chroma.iter().sum();
    if total <= f32::EPSILON {
        return None;
    }
    for c in &mut chroma {
        *c /= total;
    }
    Some(chroma)
}

fn collect_partials(frames: &[Vec<f32>], size: usize, sample_rate: f32) -> Vec<Partial> {
    let half = size / 2;
    let lo_bin = ((MIN_HZ * size as f32 / sample_rate).floor() as usize).max(2);
    let hi_bin = ((MAX_HZ * size as f32 / sample_rate).ceil() as usize).min(half - 2);
    let mut partials = Vec::new();
    let mut prefix = vec![0.0_f64; half + 1];
    for (index, frame) in frames.iter().enumerate() {
        let peak = frame[lo_bin..=hi_bin]
            .iter()
            .copied()
            .fold(0.0_f32, f32::max);
        if peak <= f32::EPSILON {
            continue;
        }
        // Tonality gate: a noise-like frame (drum hit, hiss, crowd) has a
        // flat power spectrum and only chance peaks, which would vote for
        // pitch classes at random.
        let band = &frame[lo_bin..=hi_bin];
        let mean_power = band.iter().map(|m| (m * m) as f64).sum::<f64>() / band.len() as f64;
        let mean_log = band
            .iter()
            .map(|m| ((m * m) as f64).max(1e-30).ln())
            .sum::<f64>()
            / band.len() as f64;
        let flatness = if mean_power > 0.0 {
            (mean_log.exp() / mean_power) as f32
        } else {
            1.0
        };
        if flatness > MAX_FLATNESS {
            continue;
        }
        // Ignore anything 60 dB under the frame's loudest partial.
        let floor = peak * 1.0e-3;
        for (i, value) in frame.iter().enumerate() {
            prefix[i + 1] = prefix[i] + *value as f64;
        }
        let frame_start = partials.len();
        for bin in lo_bin..=hi_bin {
            let m = frame[bin];
            if m < floor || m <= frame[bin - 1] || m < frame[bin + 1] {
                continue;
            }
            let a = bin.saturating_sub(LOCAL_BINS);
            let b = (bin + LOCAL_BINS + 1).min(half);
            let local = ((prefix[b] - prefix[a]) / (b - a) as f64) as f32;
            if m < local * PROMINENCE {
                continue;
            }
            // Parabolic interpolation on log magnitude.
            let (l, c, r) = (
                frame[bin - 1].max(1e-12).ln(),
                m.max(1e-12).ln(),
                frame[bin + 1].max(1e-12).ln(),
            );
            let denom = l - 2.0 * c + r;
            let offset = if denom.abs() > 1e-9 {
                (0.5 * (l - r) / denom).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            let freq = (bin as f32 + offset) * sample_rate / size as f32;
            partials.push(Partial {
                frame: index as u32,
                midi: 69.0 + 12.0 * (freq / 440.0).log2(),
                // Compressed so one loud partial cannot outvote a chord.
                weight: (m / peak).sqrt(),
            });
        }
        suppress_harmonics(&mut partials[frame_start..]);
    }
    partials
}

/// Harmonics that land on another pitch class — the 3rd and 6th (a fifth
/// up), 5th (a major third up) and 7th (a minor seventh up) — are what pull a
/// key estimate toward its dominant or its parallel major. A partial sitting
/// on one of those harmonics of a lower, comparably strong partial in the same
/// frame is most likely that note's overtone, so it keeps only a fraction of
/// its vote. Octave partials share the pitch class and are left alone.
fn suppress_harmonics(frame: &mut [Partial]) {
    const HARMONICS: [f32; 4] = [3.0, 5.0, 6.0, 7.0];
    for upper in 1..frame.len() {
        let (lower_part, rest) = frame.split_at_mut(upper);
        let partial = &mut rest[0];
        let overtone = lower_part.iter().any(|root| {
            root.weight >= partial.weight * 0.5
                && HARMONICS.iter().any(|&h| {
                    (partial.midi - (root.midi + 12.0 * h.log2())).abs() < HARMONIC_TOLERANCE
                })
        });
        if overtone {
            partial.weight *= HARMONIC_WEIGHT;
        }
    }
}

/// Global tuning offset in semitones (`-0.5..0.5`): the weighted circular
/// mean of every partial's distance from the equal-tempered grid.
fn estimate_tuning(partials: &[Partial]) -> f32 {
    let (mut x, mut y) = (0.0_f64, 0.0_f64);
    for partial in partials {
        let angle = std::f64::consts::TAU * partial.midi as f64;
        x += partial.weight as f64 * angle.cos();
        y += partial.weight as f64 * angle.sin();
    }
    if x.abs() + y.abs() <= f64::EPSILON {
        return 0.0;
    }
    (y.atan2(x) / std::f64::consts::TAU) as f32
}

/// Rank all 24 keys against a normalized pitch-class profile, best first.
///
/// The winner's confidence is its correlation margin over the runner-up;
/// alternates carry the winner's confidence scaled by their own correlation
/// relative to the winner, so the list reads in one consistent order.
pub fn rank_keys(chroma: &[f32; 12]) -> Vec<KeyEstimate> {
    let ranked = key_correlations(chroma);
    let best = ranked[0].0;
    let margin = if best > 0.0 {
        ((best - ranked[1].0) / best).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ranked
        .into_iter()
        .map(|(score, tonic, mode)| KeyEstimate {
            tonic,
            mode,
            confidence: if best > 0.0 {
                (margin * (score / best)).clamp(0.0, 1.0)
            } else {
                0.0
            },
        })
        .collect()
}

/// All 24 keys with their raw profile correlation (`-1..1`), best first — the
/// scores [`rank_keys`] ranks, for diagnostics.
pub fn key_correlations(chroma: &[f32; 12]) -> Vec<(f32, PitchClass, KeyMode)> {
    let mut ranked: Vec<(f32, PitchClass, KeyMode)> = Vec::with_capacity(24);
    for tonic in 0..12 {
        for (mode, profile) in [
            (KeyMode::Major, &MAJOR_PROFILE),
            (KeyMode::Minor, &MINOR_PROFILE),
        ] {
            let score = correlation(chroma, profile, tonic);
            ranked.push((score, PitchClass::from_index(tonic as i32), mode));
        }
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
    ranked
}

/// Pearson correlation between the chroma vector and a profile rotated so that
/// `tonic` aligns with the profile's tonic (index 0).
fn correlation(chroma: &[f32; 12], profile: &[f32; 12], tonic: usize) -> f32 {
    let mut rotated = [0.0_f32; 12];
    for i in 0..12 {
        rotated[i] = profile[(i + 12 - tonic) % 12];
    }

    let mean_c = chroma.iter().sum::<f32>() / 12.0;
    let mean_p = rotated.iter().sum::<f32>() / 12.0;

    let mut num = 0.0_f32;
    let mut den_c = 0.0_f32;
    let mut den_p = 0.0_f32;
    for i in 0..12 {
        let dc = chroma[i] - mean_c;
        let dp = rotated[i] - mean_p;
        num += dc * dp;
        den_c += dc * dc;
        den_p += dp * dp;
    }

    let den = (den_c * den_p).sqrt();
    if den <= f32::EPSILON { 0.0 } else { num / den }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    const SR: f32 = 44_100.0;

    fn midi_hz(midi: f32) -> f32 {
        440.0 * 2f32.powf((midi - 69.0) / 12.0)
    }

    /// A progression of harmonic-rich chords (sawtooth-like partials, which
    /// is what puts a major third on every root's fifth partial) over a bass
    /// note and a four-on-the-floor kick, `detune` semitones off A440.
    fn progression(chords: &[[f32; 4]], detune: f32) -> Vec<f32> {
        let chord_seconds = 2.0;
        let n = (SR * chord_seconds * chords.len() as f32 * 2.0) as usize;
        let mut out = vec![0.0f32; n];
        for (i, sample) in out.iter_mut().enumerate() {
            let t = i as f32 / SR;
            let chord = &chords[(t / chord_seconds) as usize % chords.len()];
            let mut v = 0.0;
            for (voice, &midi) in chord.iter().enumerate() {
                let f = midi_hz(midi + detune);
                let gain = if voice == 0 { 0.8 } else { 0.35 };
                for k in 1..=8 {
                    v += gain / k as f32 * (2.0 * PI * f * k as f32 * t).sin();
                }
            }
            let beat = t % 0.5;
            v += 1.5
                * (-beat * 25.0).exp()
                * (2.0 * PI * (50.0 + 90.0 * (-beat * 40.0).exp()) * beat).sin();
            *sample = v * 0.2;
        }
        out
    }

    fn top(samples: &[f32]) -> KeyEstimate {
        estimate_key(samples, SR).expect("key")
    }

    #[test]
    fn minor_progression_over_drums_reads_as_its_minor_key() {
        // F#m – Bm – F#m – C#m (i – iv – i – v), bass on each root.
        let chords = [
            [42.0, 66.0, 69.0, 73.0],
            [47.0, 66.0, 71.0, 74.0],
            [42.0, 66.0, 69.0, 73.0],
            [37.0, 61.0, 64.0, 68.0],
        ];
        let key = top(&progression(&chords, 0.0));
        assert_eq!(
            (key.tonic, key.mode),
            (PitchClass::Fs, KeyMode::Minor),
            "{key:?}"
        );
    }

    #[test]
    fn major_progression_reads_as_its_major_key() {
        // C – G – Am – F.
        let chords = [
            [36.0, 60.0, 64.0, 67.0],
            [43.0, 62.0, 67.0, 71.0],
            [45.0, 60.0, 64.0, 69.0],
            [41.0, 60.0, 65.0, 69.0],
        ];
        let key = top(&progression(&chords, 0.0));
        assert_eq!(
            (key.tonic, key.mode),
            (PitchClass::C, KeyMode::Major),
            "{key:?}"
        );
    }

    #[test]
    fn detuned_recording_is_read_after_tuning_correction() {
        // The same C major progression, 40 cents sharp: without tuning
        // correction half the partials round to the wrong semitone.
        let chords = [
            [36.0, 60.0, 64.0, 67.0],
            [43.0, 62.0, 67.0, 71.0],
            [45.0, 60.0, 64.0, 69.0],
            [41.0, 60.0, 65.0, 69.0],
        ];
        let key = top(&progression(&chords, 0.4));
        assert_eq!(
            (key.tonic, key.mode),
            (PitchClass::C, KeyMode::Major),
            "{key:?}"
        );
    }

    #[test]
    fn drums_alone_have_no_pitch_class_bias_toward_one_key() {
        // Noise hits carry no key; whatever comes out must not be a
        // confident answer.
        let mut state = 99u32;
        let drums: Vec<f32> = (0..(SR * 8.0) as usize)
            .map(|i| {
                let t = i as f32 / SR;
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                (-(t % 0.25) * 60.0).exp() * noise
            })
            .collect();
        if let Some(key) = estimate_key(&drums, SR) {
            assert!(key.confidence < 0.1, "{key:?}");
        }
    }

    #[test]
    fn alternates_rank_below_the_winner() {
        let chords = [[45.0, 57.0, 60.0, 64.0]];
        let keys = estimate_key_ranked(&progression(&chords, 0.0), SR);
        assert_eq!(keys.len(), 24);
        assert!(keys.windows(2).all(|w| w[0].confidence >= w[1].confidence));
    }
}

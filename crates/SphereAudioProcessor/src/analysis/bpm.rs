//! Tempo (BPM) estimation: log-band spectral-flux onset envelope, one global
//! autocorrelation, a comb over beat multiples, and a log-normal tempo prior.
//!
//! Why each piece is there:
//!
//! * **Log-compressed band flux.** Linear magnitude flux is dominated by the
//!   loudest low band, so a sustained bass swamps the hats and snares that
//!   actually carry the pulse. Compressing each band first lets every band vote.
//! * **Comb over multiples.** A periodic envelope correlates at every multiple
//!   of its period, so the raw autocorrelation maximum has no reason to be the
//!   beat — with a `(len - lag)` normalisation it even drifts toward the
//!   *longest* lag, which is how a 120 BPM loop used to read as the 60 BPM
//!   floor. Summing the (biased) autocorrelation at `P, 2P, 3P, 4P` rewards the
//!   true period over both its half and its double.
//! * **Tempo prior.** Octave ambiguity is real (70 vs 140 is often a matter of
//!   taste), so the tie is broken by a log-normal prior centred on 120 BPM with
//!   a one-octave spread, as in common beat trackers. The other octave is still
//!   returned as a candidate.
//! * **Long-lag refinement.** One frame at ~172 fps is ~1.4 BPM at 120; the
//!   period is re-measured at the 8- or 16-beat autocorrelation peak, which
//!   divides the quantisation error by that multiple.
//!
//! Offline / control-thread only.

use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};

use super::spectrum::magnitude_frames;

/// Estimated tempo of an audio buffer.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoEstimate {
    /// Beats per minute.
    pub bpm: f32,
    /// Periodicity strength of the onset envelope at this tempo, in `[0, 1]`.
    pub confidence: f32,
}

/// Analysis frame (~23 ms at 44.1 kHz) and target envelope rate.
const FRAME_SIZE: usize = 1024;
const TARGET_FPS: f32 = 172.0;
/// Onset bands: log-spaced between these edges.
const BANDS: usize = 36;
const BAND_LO_HZ: f32 = 40.0;
const BAND_HI_HZ: f32 = 12_000.0;
/// Weights of the beat multiples `P, 2P, 3P, 4P` in the comb. Falling with
/// the multiple, so a tempo is judged mostly by its own beat and its bar
/// rather than by a long lag that a triplet grid happens to share (a 112 BPM
/// track read as 149.3 = 112 x 4/3 under equal weights).
const COMB_WEIGHTS: [f32; 4] = [1.0, 0.5, 1.0 / 3.0, 0.25];
/// Most candidates offered.
const MAX_CANDIDATES: usize = 5;
/// Candidates closer than this (relative) are the same tempo.
const DISTINCT: f32 = 0.03;
/// Log-normal tempo prior.
const PRIOR_CENTRE_BPM: f32 = 120.0;
const PRIOR_OCTAVES: f32 = 1.0;
/// Length of each local autocorrelation window.
const TEMPOGRAM_SECONDS: f32 = 8.0;
/// Coarse search step.
const GRID_BPM: f32 = 0.1;
/// Mean onset flux per band, relative to the mean log band level, below which
/// there is no pulse to measure. Music sits around 0.02–0.08; a steady tone
/// near 0.008.
const MIN_ONSET_RATIO: f32 = 0.015;
/// A refined tempo this close to a whole number is reported as that number.
const SNAP_BPM: f32 = 0.06;

/// Estimate tempo of a mono buffer. Returns `None` for signals too short or
/// too flat to hold a meaningful onset envelope.
pub fn estimate_bpm(
    samples: &[f32],
    sample_rate: f32,
    min_bpm: f32,
    max_bpm: f32,
) -> Option<TempoEstimate> {
    let analysis = TempoAnalysis::new(samples, sample_rate)?;
    let (min_bpm, max_bpm) = sanitize_range(min_bpm, max_bpm);
    let peaks = analysis.peaks_in(min_bpm, max_bpm);
    let &(coarse, _) = peaks.first()?;
    let bpm = analysis.refine(coarse);
    Some(TempoEstimate {
        bpm,
        confidence: analysis.confidence(coarse, &peaks),
    })
}

/// Ranked tempo candidates: the winner plus musically related doubles/halves.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoCandidate {
    pub bpm: f32,
    pub confidence: f32,
}

/// The winner, the other distinct tempos the audio supports (local maxima of
/// the search, e.g. a 4:3 triplet reading), and the winner's half and double
/// when inside the range. Each carries its own measured confidence scaled by
/// how the search ranked it. Sorted by confidence, best first.
pub fn estimate_bpm_candidates(
    samples: &[f32],
    sample_rate: f32,
    min_bpm: f32,
    max_bpm: f32,
) -> Vec<TempoCandidate> {
    let Some(analysis) = TempoAnalysis::new(samples, sample_rate) else {
        return Vec::new();
    };
    let (min_bpm, max_bpm) = sanitize_range(min_bpm, max_bpm);
    let peaks = analysis.peaks_in(min_bpm, max_bpm);
    let Some(&(coarse, best_score)) = peaks.first() else {
        return Vec::new();
    };
    let best = analysis.refine(coarse);
    let best_conf = analysis.confidence(coarse, &peaks);
    let mut candidates = vec![TempoCandidate {
        bpm: best,
        confidence: best_conf,
    }];
    let distinct = |candidates: &[TempoCandidate], bpm: f32| {
        candidates
            .iter()
            .all(|c| ((c.bpm - bpm) / bpm).abs() > DISTINCT)
    };
    let relative = |bpm: f32| {
        (best_conf * (analysis.score(bpm) / best_score).clamp(0.0, 0.99)).clamp(0.0, 1.0)
    };
    for ratio in [0.5_f32, 2.0] {
        let bpm = snap(best * ratio);
        if bpm >= min_bpm && bpm <= max_bpm && distinct(&candidates, bpm) {
            candidates.push(TempoCandidate {
                bpm,
                confidence: relative(bpm),
            });
        }
    }
    for &(coarse, _) in peaks.iter().skip(1) {
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let bpm = analysis.refine(coarse);
        if distinct(&candidates, bpm) {
            candidates.push(TempoCandidate {
                bpm,
                confidence: relative(bpm),
            });
        }
    }
    candidates.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    candidates
}

/// Onset envelope and its normalised autocorrelation, computed once.
struct TempoAnalysis {
    /// Envelope frames per second.
    fps: f32,
    /// Biased autocorrelation, `acf[0] == 1`.
    acf: Vec<f32>,
    /// Mean of per-window normalised autocorrelations (a tempogram averaged
    /// over time), so one section cannot dominate the tempo choice.
    local: Vec<f32>,
}

impl TempoAnalysis {
    fn new(samples: &[f32], sample_rate: f32) -> Option<Self> {
        if sample_rate <= 0.0 || !sample_rate.is_finite() {
            return None;
        }
        let hop = ((sample_rate / TARGET_FPS).round() as usize).max(64);
        let fps = sample_rate / hop as f32;
        let envelope = onset_envelope(samples, sample_rate, hop)?;
        let acf = autocorrelation(&envelope)?;
        let local = windowed_autocorrelation(&envelope, fps).unwrap_or_else(|| acf.clone());
        Some(Self { fps, acf, local })
    }

    /// Autocorrelation at a fractional lag (frames), linearly interpolated.
    fn acf_at(&self, lag: f32) -> Option<f32> {
        let src = &self.local;
        if lag < 1.0 {
            return None;
        }
        let i = lag.floor() as usize;
        if i + 1 >= src.len() {
            return None;
        }
        let t = lag - i as f32;
        Some(src[i] * (1.0 - t) + src[i + 1] * t)
    }

    fn lag_for(&self, bpm: f32) -> f32 {
        self.fps * 60.0 / bpm
    }

    /// Mean autocorrelation over the beat multiples that fit the envelope.
    fn periodicity(&self, bpm: f32) -> f32 {
        let lag = self.lag_for(bpm);
        let mut sum = 0.0;
        let mut count = 0.0;
        for (k, w) in COMB_WEIGHTS.iter().enumerate() {
            if let Some(value) = self.acf_at(lag * (k + 1) as f32) {
                sum += value * w;
                count += w;
            }
        }
        if count == 0.0 { 0.0 } else { sum / count }
    }

    /// Periodicity weighted by the tempo prior — what the search maximises.
    fn score(&self, bpm: f32) -> f32 {
        let octaves = (bpm / PRIOR_CENTRE_BPM).log2() / PRIOR_OCTAVES;
        self.periodicity(bpm).max(0.0) * (-0.5 * octaves * octaves).exp()
    }

    /// Local maxima of the score over the range, strongest first.
    fn peaks_in(&self, min_bpm: f32, max_bpm: f32) -> Vec<(f32, f32)> {
        let steps = ((max_bpm - min_bpm) / GRID_BPM).round() as usize;
        let scores: Vec<(f32, f32)> = (0..=steps)
            .map(|step| {
                let bpm = min_bpm + step as f32 * GRID_BPM;
                (bpm, self.score(bpm))
            })
            .collect();
        let mut peaks: Vec<(f32, f32)> = (0..scores.len())
            .filter(|&i| {
                let s = scores[i].1;
                s > 0.0
                    && (i == 0 || s >= scores[i - 1].1)
                    && (i + 1 == scores.len() || s > scores[i + 1].1)
            })
            .map(|i| scores[i])
            .collect();
        peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
        peaks
    }

    fn best_in(&self, min_bpm: f32, max_bpm: f32) -> Option<(f32, f32)> {
        let mut best: Option<(f32, f32)> = None;
        let steps = ((max_bpm - min_bpm) / GRID_BPM).round() as usize;
        for step in 0..=steps {
            let bpm = min_bpm + step as f32 * GRID_BPM;
            let score = self.score(bpm);
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((bpm, score));
            }
        }
        best.filter(|(_, score)| *score > 0.0)
    }

    /// Re-measure the period at the longest beat multiple whose
    /// autocorrelation peak is still clear, then snap near-whole tempos.
    fn refine(&self, bpm: f32) -> f32 {
        let lag = self.lag_for(bpm);
        let limit = self.acf.len() as f32 * 0.5;
        for k in [16_usize, 8, 4, 2, 1] {
            let centre = lag * k as f32;
            if centre + 3.0 >= limit {
                continue;
            }
            let reach = (centre * 0.02).max(2.0);
            let lo = (centre - reach).floor().max(1.0) as usize;
            let hi = ((centre + reach).ceil() as usize).min(self.acf.len() - 2);
            let Some(peak) = (lo..=hi).max_by(|&a, &b| self.acf[a].total_cmp(&self.acf[b])) else {
                continue;
            };
            // Only a true interior maximum measures the period.
            if peak == lo || peak == hi || self.acf[peak] <= 0.0 {
                continue;
            }
            let (a, b, c) = (self.acf[peak - 1], self.acf[peak], self.acf[peak + 1]);
            let denom = a - 2.0 * b + c;
            let offset = if denom.abs() > 1e-12 {
                (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
            } else {
                0.0
            };
            let refined = (peak as f32 + offset) / k as f32;
            if ((refined - lag) / lag).abs() < 0.03 {
                return snap(self.fps * 60.0 / refined);
            }
        }
        snap(bpm)
    }

    /// Confidence in the winner, `[0, 1]`: the geometric mean of
    ///
    /// * **pulse** — how strongly the envelope repeats at the winner (dense
    ///   mixes with a clear groove reach about 0.2 weighted autocorrelation;
    ///   a click loop reaches far more), and
    /// * **dominance** — its margin over the strongest peak that is not simply
    ///   its half or double. Octave doubt is left to the listener (both
    ///   octaves are offered); a competing 4:3 or unrelated tempo is real doubt.
    fn confidence(&self, best: f32, peaks: &[(f32, f32)]) -> f32 {
        let best_score = self.score(best);
        if best_score <= 0.0 {
            return 0.0;
        }
        let pulse = (self.periodicity(best) / 0.2).clamp(0.0, 1.0);
        let octave = |bpm: f32| {
            [0.5_f32, 1.0, 2.0]
                .iter()
                .any(|r| ((bpm - best * r) / (best * r)).abs() <= DISTINCT)
        };
        let competitor = peaks
            .iter()
            .filter(|(bpm, _)| !octave(*bpm))
            .map(|(_, score)| *score)
            .fold(0.0_f32, f32::max);
        let dominance = (1.0 - competitor / best_score).clamp(0.0, 1.0);
        (pulse * dominance).sqrt()
    }
}

fn snap(bpm: f32) -> f32 {
    let whole = bpm.round();
    if (bpm - whole).abs() <= SNAP_BPM {
        whole
    } else {
        (bpm * 100.0).round() / 100.0
    }
}

/// Log-compressed band spectral flux, high-passed and normalised.
fn onset_envelope(samples: &[f32], sample_rate: f32, hop: usize) -> Option<Vec<f32>> {
    let frames = magnitude_frames(samples, FRAME_SIZE, hop);
    if frames.len() < 16 {
        return None;
    }
    let half = FRAME_SIZE / 2;
    let hz_per_bin = sample_rate / FRAME_SIZE as f32;
    let top = BAND_HI_HZ.min(sample_rate * 0.45);
    let edges: Vec<usize> = (0..=BANDS)
        .map(|i| {
            let hz = BAND_LO_HZ * (top / BAND_LO_HZ).powf(i as f32 / BANDS as f32);
            ((hz / hz_per_bin).round() as usize).clamp(1, half)
        })
        .collect();

    let mut bands = vec![0.0_f32; frames.len() * BANDS];
    let mut total = 0.0_f64;
    for (t, frame) in frames.iter().enumerate() {
        for b in 0..BANDS {
            let lo = edges[b];
            let hi = edges[b + 1].max(lo + 1).min(half);
            let slice = &frame[lo.min(hi - 1)..hi];
            let mean = slice.iter().sum::<f32>() / slice.len() as f32;
            bands[t * BANDS + b] = mean;
            total += mean as f64;
        }
    }
    let mean = (total / bands.len() as f64) as f32;
    if mean <= f32::EPSILON {
        return None;
    }
    // Level-independent compression: the reference sits well below the
    // average band level so quiet bands still register their onsets.
    let reference = mean * 0.1;
    for value in &mut bands {
        *value = (1.0 + *value / reference).ln();
    }

    let mut flux = vec![0.0_f32; frames.len()];
    for t in 1..frames.len() {
        let (prev, cur) = (
            &bands[(t - 1) * BANDS..t * BANDS],
            &bands[t * BANDS..(t + 1) * BANDS],
        );
        flux[t] = cur
            .iter()
            .zip(prev)
            .map(|(c, p)| (c - p).max(0.0))
            .sum::<f32>();
    }
    // A steady tone still has a faintly periodic envelope — window phase
    // against the waveform — and would otherwise read as a confident tempo.
    // Real onsets move the log bands by an order of magnitude more.
    let level = bands.iter().sum::<f32>() / bands.len() as f32;
    let mean_flux = flux.iter().sum::<f32>() / flux.len() as f32;
    if level <= f32::EPSILON || mean_flux / (level * BANDS as f32) < MIN_ONSET_RATIO {
        return None;
    }

    // High-pass: remove the local mean (~0.5 s) so slow loudness swells do
    // not read as periodicity, then keep only the rises.
    let fps = sample_rate / hop as f32;
    let radius = ((fps * 0.25) as usize).max(1);
    let mut prefix = vec![0.0_f64; flux.len() + 1];
    for (i, v) in flux.iter().enumerate() {
        prefix[i + 1] = prefix[i] + *v as f64;
    }
    let mut envelope: Vec<f32> = (0..flux.len())
        .map(|i| {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius + 1).min(flux.len());
            let local = ((prefix[hi] - prefix[lo]) / (hi - lo) as f64) as f32;
            (flux[i] - local).max(0.0)
        })
        .collect();
    let mean = envelope.iter().sum::<f32>() / envelope.len() as f32;
    for value in &mut envelope {
        *value -= mean;
    }
    let energy: f32 = envelope.iter().map(|v| v * v).sum();
    (energy > f32::EPSILON).then_some(envelope)
}

/// Average of normalised autocorrelations over overlapping windows.
fn windowed_autocorrelation(envelope: &[f32], fps: f32) -> Option<Vec<f32>> {
    let window = (fps * TEMPOGRAM_SECONDS) as usize;
    if envelope.len() < window * 3 / 2 {
        return None;
    }
    let hop = (window / 4).max(1);
    let mut sum = vec![0.0_f32; window];
    let mut count = 0;
    let mut start = 0;
    while start + window <= envelope.len() {
        let slice = &envelope[start..start + window];
        let mean = slice.iter().sum::<f32>() / window as f32;
        let centred: Vec<f32> = slice.iter().map(|v| v - mean).collect();
        if let Some(acf) = autocorrelation(&centred) {
            for (acc, value) in sum.iter_mut().zip(acf) {
                *acc += value;
            }
            count += 1;
        }
        start += hop;
    }
    (count > 0).then(|| sum.into_iter().map(|v| v / count as f32).collect())
}

/// Biased autocorrelation via FFT, normalised so lag 0 is 1.
fn autocorrelation(signal: &[f32]) -> Option<Vec<f32>> {
    let n = signal.len();
    let size = (2 * n).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(size);
    let inverse = planner.plan_fft_inverse(size);
    let mut buf: Vec<Complex<f32>> = signal
        .iter()
        .map(|&v| Complex::new(v, 0.0))
        .chain(std::iter::repeat_n(Complex::new(0.0, 0.0), size - n))
        .collect();
    forward.process(&mut buf);
    for value in &mut buf {
        *value = Complex::new(value.norm_sqr(), 0.0);
    }
    inverse.process(&mut buf);
    let zero = buf[0].re;
    if zero <= f32::EPSILON {
        return None;
    }
    Some(buf[..n].iter().map(|c| c.re / zero).collect())
}

fn sanitize_range(min_bpm: f32, max_bpm: f32) -> (f32, f32) {
    let mut lo = if min_bpm.is_finite() && min_bpm > 0.0 {
        min_bpm
    } else {
        60.0
    };
    let mut hi = if max_bpm.is_finite() && max_bpm > lo {
        max_bpm
    } else {
        200.0
    };
    lo = lo.clamp(20.0, 400.0);
    hi = hi.clamp(lo + 1.0, 400.0);
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;

    /// Deterministic noise.
    fn noise(state: &mut u32) -> f32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }

    /// A drum loop: kick on every beat, snare on 2 and 4, closed hats on
    /// eighths, plus a sustained bass note that must not drown the pulse.
    fn drum_loop(bpm: f32, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        let beat = 60.0 / bpm;
        let mut out = vec![0.0f32; n];
        let mut state = 7u32;
        for (i, sample) in out.iter_mut().enumerate() {
            let t = i as f32 / SR;
            let in_beat = t % beat;
            let beat_index = (t / beat) as usize;
            let in_eighth = t % (beat * 0.5);
            let kick =
                (-in_beat * 30.0).exp() * (2.0 * std::f32::consts::PI * 55.0 * in_beat).sin();
            let snare = if beat_index % 2 == 1 {
                (-in_beat * 25.0).exp() * noise(&mut state) * 0.6
            } else {
                0.0
            };
            let hat = (-in_eighth * 120.0).exp() * noise(&mut state) * 0.25;
            let bass = 0.5 * (2.0 * std::f32::consts::PI * 41.2 * t).sin();
            *sample = kick + snare + hat + bass;
        }
        out
    }

    fn best(samples: &[f32]) -> f32 {
        estimate_bpm(samples, SR, 60.0, 200.0).expect("tempo").bpm
    }

    #[test]
    fn finds_the_beat_not_the_range_floor() {
        // The old estimator read this 120 BPM loop as 60.
        assert_eq!(best(&drum_loop(120.0, 20.0)), 120.0);
    }

    #[test]
    fn measures_common_tempos_to_a_tenth_of_a_bpm() {
        for bpm in [90.0_f32, 100.0, 128.0, 140.0] {
            let found = best(&drum_loop(bpm, 24.0));
            assert!((found - bpm).abs() <= 0.1, "{bpm} BPM read as {found}");
        }
    }

    /// Kick on every beat with a backbeat snare at 174 is also a valid 87
    /// half-time groove; either octave may win, but both must be offered and
    /// both must be exact.
    #[test]
    fn fast_tempos_offer_both_octaves_exactly() {
        let candidates = estimate_bpm_candidates(&drum_loop(174.0, 24.0), SR, 60.0, 200.0);
        let bpms: Vec<f32> = candidates.iter().map(|c| c.bpm).collect();
        assert!(bpms.contains(&174.0) && bpms.contains(&87.0), "{bpms:?}");
    }

    #[test]
    fn keeps_fractional_tempos() {
        let found = best(&drum_loop(123.4, 30.0));
        assert!((found - 123.4).abs() <= 0.1, "123.4 BPM read as {found}");
    }

    #[test]
    fn candidates_offer_the_other_octave() {
        let candidates = estimate_bpm_candidates(&drum_loop(100.0, 20.0), SR, 60.0, 200.0);
        assert_eq!(candidates[0].bpm, 100.0);
        assert!(candidates.iter().any(|c| c.bpm == 200.0));
        assert!(candidates[0].confidence > candidates[1].confidence);
        assert!(
            candidates[0].confidence > 0.5,
            "clear loop should be confident"
        );
    }

    #[test]
    fn steady_tone_has_no_tempo() {
        let tone: Vec<f32> = (0..(SR * 5.0) as usize)
            .map(|i| (2.0 * std::f32::consts::PI * 220.0 * i as f32 / SR).sin())
            .collect();
        let estimate = estimate_bpm(&tone, SR, 60.0, 200.0);
        assert!(estimate.is_none_or(|e| e.confidence < 0.2), "{estimate:?}");
    }
}

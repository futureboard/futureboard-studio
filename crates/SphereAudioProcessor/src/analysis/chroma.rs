//! Frame-level chroma — treble and bass — for beat-synchronous harmony.
//!
//! Built from spectral *peaks* rather than raw bins: each peak is located to a
//! fraction of a bin (parabolic interpolation on log magnitude), so even the
//! bass octave, where a semitone is narrower than an FFT bin, lands on the
//! right pitch class. The recording's overall tuning is measured first and
//! taken out, and overtones sitting on another pitch class (a fifth or a major
//! third above a stronger note) are turned down, since they are what makes a C
//! chord read as C + G + E-ish noise.
//!
//! Offline / control-thread only.

use rustfft::{FftPlanner, num_complex::Complex};

/// Analysis window, seconds (~186 ms: resolves semitones down to ~50 Hz once
/// peaks are interpolated).
const WINDOW_SECONDS: f32 = 0.186;
/// Frame hop, seconds. Chords are summarised per beat, so this only has to be
/// comfortably finer than the fastest beat.
pub const CHROMA_HOP_SECONDS: f32 = 0.046;
const TREBLE_HZ: (f32, f32) = (110.0, 2_100.0);
const BASS_HZ: (f32, f32) = (40.0, 260.0);
/// Peaks this far under the frame's loudest are ignored.
const FLOOR: f32 = 1.0e-3;
/// Peaks must stand this far above their neighbourhood mean.
const PROMINENCE: f32 = 2.0;
const LOCAL_BINS: usize = 16;
/// Overtones on another pitch class keep this share of their weight.
const OVERTONE_WEIGHT: f32 = 0.35;

#[derive(Clone, Copy, Debug)]
struct Peak {
    midi: f32,
    weight: f32,
    hz: f32,
}

/// Chroma per analysis frame. Each vector is non-negative; all-zero frames
/// hold no pitched energy.
#[derive(Clone, Debug, Default)]
pub struct ChromaFrames {
    pub hop_seconds: f64,
    /// Offset of frame 0's centre, seconds.
    pub start_seconds: f64,
    pub treble: Vec<[f32; 12]>,
    pub bass: Vec<[f32; 12]>,
    /// Broadband level per frame (linear), for silence and no-chord tests.
    pub energy: Vec<f32>,
    /// Tuning offset that was removed, semitones (`-0.5..0.5`).
    pub tuning: f32,
}

impl ChromaFrames {
    /// Frame index nearest `seconds`.
    pub fn frame_at(&self, seconds: f64) -> usize {
        (((seconds - self.start_seconds) / self.hop_seconds).round().max(0.0) as usize)
            .min(self.treble.len().saturating_sub(1))
    }

    /// Summed chroma over `[start, end)` seconds: `(treble, bass, energy)`.
    pub fn span(&self, start: f64, end: f64) -> ([f32; 12], [f32; 12], f32) {
        let mut treble = [0.0_f32; 12];
        let mut bass = [0.0_f32; 12];
        let mut energy = 0.0_f32;
        if self.treble.is_empty() {
            return (treble, bass, energy);
        }
        let a = self.frame_at(start);
        let b = self.frame_at(end).max(a + 1).min(self.treble.len());
        for i in a..b {
            for pc in 0..12 {
                treble[pc] += self.treble[i][pc];
                bass[pc] += self.bass[i][pc];
            }
            energy += self.energy[i];
        }
        let n = (b - a).max(1) as f32;
        (treble, bass, energy / n)
    }
}

pub fn chroma_frames(samples: &[f32], sample_rate: f32) -> Option<ChromaFrames> {
    if sample_rate <= 0.0 || !sample_rate.is_finite() {
        return None;
    }
    let size = ((sample_rate * WINDOW_SECONDS) as usize).next_power_of_two().max(2048);
    let hop = ((sample_rate * CHROMA_HOP_SECONDS) as usize).max(1);
    if samples.len() < size {
        return None;
    }
    let half = size / 2;
    let window: Vec<f32> = (0..size)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / size as f32).cos())
        .collect();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(size);
    let mut buf = vec![Complex::new(0.0_f32, 0.0); size];
    let mut scratch = vec![Complex::new(0.0_f32, 0.0); fft.get_inplace_scratch_len()];
    let mut mag = vec![0.0_f32; half];
    let mut prefix = vec![0.0_f64; half + 1];

    let bin_of = |hz: f32| (hz * size as f32 / sample_rate) as usize;
    let lo = bin_of(BASS_HZ.0).max(2);
    let hi = bin_of(TREBLE_HZ.1).min(half - 2);

    let mut frame_peaks: Vec<Vec<Peak>> = Vec::new();
    let mut energy = Vec::new();
    let mut pos = 0;
    while pos + size <= samples.len() {
        for i in 0..size {
            buf[i] = Complex::new(samples[pos + i] * window[i], 0.0);
        }
        fft.process_with_scratch(&mut buf, &mut scratch);
        let mut total = 0.0_f32;
        for (i, m) in mag.iter_mut().enumerate() {
            *m = buf[i].norm();
            total += *m;
        }
        energy.push(total / half as f32);
        for (i, m) in mag.iter().enumerate() {
            prefix[i + 1] = prefix[i] + *m as f64;
        }
        let loudest = mag[lo..=hi].iter().copied().fold(0.0_f32, f32::max);
        let mut peaks = Vec::new();
        if loudest > f32::EPSILON {
            let floor = loudest * FLOOR;
            for bin in lo..=hi {
                let m = mag[bin];
                if m < floor || m <= mag[bin - 1] || m < mag[bin + 1] {
                    continue;
                }
                let a = bin.saturating_sub(LOCAL_BINS);
                let b = (bin + LOCAL_BINS + 1).min(half);
                let local = ((prefix[b] - prefix[a]) / (b - a) as f64) as f32;
                if m < local * PROMINENCE {
                    continue;
                }
                let (l, c, r) = (
                    mag[bin - 1].max(1e-12).ln(),
                    m.max(1e-12).ln(),
                    mag[bin + 1].max(1e-12).ln(),
                );
                let denom = l - 2.0 * c + r;
                let offset = if denom.abs() > 1e-9 {
                    (0.5 * (l - r) / denom).clamp(-0.5, 0.5)
                } else {
                    0.0
                };
                let hz = (bin as f32 + offset) * sample_rate / size as f32;
                peaks.push(Peak {
                    midi: 69.0 + 12.0 * (hz / 440.0).log2(),
                    weight: (m / loudest).sqrt(),
                    hz,
                });
            }
            suppress_overtones(&mut peaks);
        }
        frame_peaks.push(peaks);
        pos += hop;
    }
    if frame_peaks.is_empty() {
        return None;
    }

    let tuning = estimate_tuning(&frame_peaks);
    let mut treble = Vec::with_capacity(frame_peaks.len());
    let mut bass = Vec::with_capacity(frame_peaks.len());
    for peaks in &frame_peaks {
        let mut t = [0.0_f32; 12];
        let mut b = [0.0_f32; 12];
        for peak in peaks {
            let tuned = peak.midi - tuning;
            let nearest = tuned.round();
            let off = (tuned - nearest).abs();
            if off > 0.4 {
                continue;
            }
            // Full weight on pitch, fading toward a quarter-tone off.
            let w = peak.weight * (1.0 - off / 0.5).max(0.0);
            let pc = (nearest as i32).rem_euclid(12) as usize;
            if (TREBLE_HZ.0..=TREBLE_HZ.1).contains(&peak.hz) {
                t[pc] += w;
            }
            if (BASS_HZ.0..=BASS_HZ.1).contains(&peak.hz) {
                b[pc] += w;
            }
        }
        treble.push(t);
        bass.push(b);
    }
    Some(ChromaFrames {
        hop_seconds: hop as f64 / sample_rate as f64,
        start_seconds: size as f64 * 0.5 / sample_rate as f64,
        treble,
        bass,
        energy,
        tuning,
    })
}

/// Overtones landing on another pitch class — the 3rd and 6th harmonic (a
/// fifth up), the 5th (a major third up) and the 7th — keep only part of
/// their weight when a comparably strong fundamental sits below them.
fn suppress_overtones(peaks: &mut [Peak]) {
    const HARMONICS: [f32; 4] = [3.0, 5.0, 6.0, 7.0];
    for upper in 1..peaks.len() {
        let (lower, rest) = peaks.split_at_mut(upper);
        let peak = &mut rest[0];
        let overtone = lower.iter().any(|root| {
            root.weight >= peak.weight * 0.5
                && HARMONICS
                    .iter()
                    .any(|&h| (peak.midi - (root.midi + 12.0 * h.log2())).abs() < 0.3)
        });
        if overtone {
            peak.weight *= OVERTONE_WEIGHT;
        }
    }
}

/// Weighted circular mean of every peak's distance from the equal-tempered
/// grid, in semitones.
fn estimate_tuning(frames: &[Vec<Peak>]) -> f32 {
    let (mut x, mut y) = (0.0_f64, 0.0_f64);
    for peak in frames.iter().flatten() {
        let angle = std::f64::consts::TAU * peak.midi as f64;
        x += peak.weight as f64 * angle.cos();
        y += peak.weight as f64 * angle.sin();
    }
    if x.abs() + y.abs() <= f64::EPSILON {
        return 0.0;
    }
    (y.atan2(x) / std::f64::consts::TAU) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freqs: &[f32], seconds: f32, sr: f32) -> Vec<f32> {
        (0..(seconds * sr) as usize)
            .map(|i| {
                let t = i as f32 / sr;
                freqs
                    .iter()
                    .map(|f| (std::f32::consts::TAU * f * t).sin())
                    .sum::<f32>()
                    * 0.2
            })
            .collect()
    }

    fn top3(c: &[f32; 12]) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..12).collect();
        idx.sort_by(|a, b| c[*b].total_cmp(&c[*a]));
        let mut top = idx[..3].to_vec();
        top.sort();
        top
    }

    #[test]
    fn a_c_major_triad_lights_c_e_g_and_the_bass_note() {
        let sr = 44_100.0;
        // C3 bass + C4 E4 G4.
        let audio = tone(&[130.81, 261.63, 329.63, 392.0], 2.0, sr);
        let frames = chroma_frames(&audio, sr).unwrap();
        let (treble, bass, _) = frames.span(0.5, 1.5);
        assert_eq!(top3(&treble), vec![0, 4, 7]);
        let bass_pc = (0..12).max_by(|a, b| bass[*a].total_cmp(&bass[*b])).unwrap();
        assert_eq!(bass_pc, 0);
    }

    #[test]
    fn detuned_audio_still_lands_on_the_right_pitch_classes() {
        let sr = 44_100.0;
        // A minor, 30 cents sharp.
        let sharp = 2f32.powf(0.3 / 12.0);
        let audio = tone(&[220.0 * sharp, 261.63 * sharp, 329.63 * sharp], 2.0, sr);
        let frames = chroma_frames(&audio, sr).unwrap();
        assert!((frames.tuning - 0.3).abs() < 0.08, "{}", frames.tuning);
        let (treble, _, _) = frames.span(0.5, 1.5);
        assert_eq!(top3(&treble), vec![0, 4, 9]);
    }
}

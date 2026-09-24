//! Beats, downbeats, meter and a tempo map — for music whose tempo moves.
//!
//! One global BPM describes a click track, not a band. This follows the
//! pipeline beat-tracking research settled on, in plain DSP:
//!
//! 1. **Onset strength.** Log-compressed band spectral flux at ~100 frames/s,
//!    plus a separate low band (kick drum) for the downbeat stage.
//! 2. **Local tempo.** An autocorrelation tempogram over 8 s windows scores
//!    every tempo on a 0.5 % grid; a Viterbi pass picks the path that explains
//!    the tempogram with the fewest, smallest tempo moves, with an octave
//!    prior at 120 BPM so each section settles on its natural beat level.
//! 3. **Beats.** Dynamic programming (Ellis 2007): each beat is the onset that
//!    best continues the previous beat one *local* period later, so the grid
//!    bends with the tempo instead of assuming one.
//! 4. **Meter and downbeats.** Bars start where the kick lands, the harmony
//!    changes and the accents fall. An HMM walks bar positions over the beats
//!    for 3 and 4 beats per bar and keeps the meter that explains the evidence
//!    better; a missing or extra beat costs a phase reset, not the whole song.
//! 5. **Tempo sections.** Per-bar tempo is segmented into constant stretches
//!    by optimal change-point search, so a song that moves from 90 to 140 BPM
//!    reads as two sections, not as a 115 BPM average.
//! 6. **Steady sections.** Most records are played to a click. Where one
//!    constant grid explains a section's beats, the beats are locked to it —
//!    its tempo snapped to a whole BPM when that fits just as well — and the
//!    bars are found again on the locked beats. A tracker that briefly
//!    follows an off-beat kick, or wobbles by a few ms, then reads as the one
//!    tempo the song was produced at; sections that really drift keep their
//!    tracked beats.
//!
//! Offline / control-thread only.

use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};

use super::chroma::ChromaFrames;

/// Onset frames per second.
const ONSET_FPS: f32 = 100.0;
const BANDS: usize = 36;
const BAND_LO_HZ: f32 = 30.0;
const BAND_HI_HZ: f32 = 16_000.0;
/// Upper edge of the "kick" band.
const LOW_BAND_HZ: f32 = 150.0;
/// Tempogram window and hop, seconds.
const TEMPO_WINDOW_SECONDS: f32 = 8.0;
const TEMPO_HOP_SECONDS: f32 = 1.0;
/// Tempo grid spacing (log): 0.5 %.
const TEMPO_GRID_RATIO: f32 = 1.005;
/// Comb weights over period multiples when scoring a tempo.
const COMB: [f32; 4] = [1.0, 0.5, 1.0 / 3.0, 0.25];
/// Cost of a tempo move between tempogram windows, per (ln ratio)².
const TEMPO_TRANSITION: f32 = 60.0;
/// Weight of the per-window octave prior (log-normal at 120 BPM).
const TEMPO_PRIOR: f32 = 0.5;
/// Beat DP tightness: how strongly a beat must sit one period after the last.
/// Tuned on GTZAN; 200 held on Ballroom and Candombe too.
const TIGHTNESS: f32 = 200.0;
/// Downbeat evidence weights (z-scored features), tuned on GTZAN and checked
/// on Ballroom and Candombe: harmonic change dominates, the kick helps, raw
/// accent strength does not (hats and snares are loud off the downbeat).
const W_ACCENT: f32 = 0.0;
const W_KICK: f32 = 0.3;
const W_HARMONY: f32 = 1.5;
/// Beats of context either side when measuring harmonic change.
const HARMONY_CONTEXT: isize = 4;
/// Probability per beat that the bar position jumps (a dropped or added beat).
const BAR_RESET: f32 = 0.02;
/// Per-beat preference for 4 over 3 beats per bar when the evidence is close.
const FOUR_BIAS: f32 = 0.04;
/// Tempo change worth a new section: roughly 2 % held for 8 bars.
const SECTION_PENALTY: f32 = 0.0032;
const MIN_BEATS: usize = 8;
/// A beat within this share of a period of its grid line counts as on it.
const GRID_TOLERANCE: f32 = 0.07;
/// Share of a section's beats that must sit on one constant grid to lock it.
/// A beat half a period off counts: the tracker briefly following an
/// off-beat kick still heard the same tempo, only the wrong phase.
const STEADY_SHARE: f32 = 0.75;
/// A whole-number tempo is used when it keeps this share of the free fit's
/// beats consistent with the grid, give or take [`SNAP_SLACK`] beats (a short
/// intro can lose one messy beat at its edge and still be a whole tempo).
const SNAP_KEEP: f32 = 0.97;
const SNAP_SLACK: f32 = 2.0;
/// Neighbouring steady sections closer than this (tempo ratio) are tried as
/// one.
const MERGE_RATIO: f64 = 0.01;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RhythmOptions {
    pub min_bpm: f32,
    pub max_bpm: f32,
    /// Force this many beats per bar instead of detecting 3 vs 4.
    pub beats_per_bar: Option<u32>,
}

impl Default for RhythmOptions {
    fn default() -> Self {
        Self {
            min_bpm: 40.0,
            max_bpm: 250.0,
            beats_per_bar: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Beat {
    pub seconds: f64,
    /// Position in the bar, 1-based: 1 is a downbeat.
    pub position: u32,
    /// Onset strength at the beat, normalised to the file (`~0..3`).
    pub strength: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoSection {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub bpm: f32,
    /// Index of the section's first beat.
    pub first_beat: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RhythmAnalysis {
    pub beats: Vec<Beat>,
    pub beats_per_bar: u32,
    /// Constant-tempo stretches, in order; one for a steady song.
    pub sections: Vec<TempoSection>,
    /// Duration-weighted tempo.
    pub bpm: f32,
    /// Whether the tempo changes enough to need a tempo map.
    pub variable: bool,
    /// Smoothed local tempo per beat: `(seconds, bpm)`, for display.
    pub tempo_curve: Vec<(f64, f32)>,
    /// How well the beats sit on onsets, `0..1`.
    pub confidence: f32,
}

impl RhythmAnalysis {
    /// Seconds of every downbeat.
    pub fn downbeats(&self) -> Vec<f64> {
        self.beats
            .iter()
            .filter(|b| b.position == 1)
            .map(|b| b.seconds)
            .collect()
    }
}

/// Onset strength envelopes at [`ONSET_FPS`].
#[derive(Clone, Debug)]
pub struct Onsets {
    pub fps: f32,
    /// Broadband flux, zero-mean-ish, unit standard deviation.
    pub flux: Vec<f32>,
    /// Flux of the bands under [`LOW_BAND_HZ`], same normalisation.
    pub low: Vec<f32>,
}

pub fn onsets(samples: &[f32], sample_rate: f32) -> Option<Onsets> {
    if sample_rate <= 0.0 || !sample_rate.is_finite() {
        return None;
    }
    let hop = ((sample_rate / ONSET_FPS).round() as usize).max(1);
    let size = ((sample_rate * 0.046) as usize)
        .next_power_of_two()
        .max(512);
    let half = size / 2;
    let fps = sample_rate / hop as f32;
    let frames = samples.len() / hop;
    if frames < 32 {
        return None;
    }
    let hz_per_bin = sample_rate / size as f32;
    let top = BAND_HI_HZ.min(sample_rate * 0.45);
    let edges: Vec<usize> = (0..=BANDS)
        .map(|i| {
            let hz = BAND_LO_HZ * (top / BAND_LO_HZ).powf(i as f32 / BANDS as f32);
            ((hz / hz_per_bin).round() as usize).clamp(1, half)
        })
        .collect();
    let low_bands = (0..BANDS)
        .take_while(|b| (edges[b + 1] as f32 * hz_per_bin) <= LOW_BAND_HZ * 1.2)
        .count()
        .max(1);

    let window: Vec<f32> = (0..size)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / size as f32).cos())
        .collect();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(size);
    let mut buf = vec![Complex::new(0.0_f32, 0.0); size];
    let mut scratch = vec![Complex::new(0.0_f32, 0.0); fft.get_inplace_scratch_len()];
    let mut bands = vec![0.0_f32; frames * BANDS];
    let mut total = 0.0_f64;
    for t in 0..frames {
        // Frame t is centred on sample t * hop.
        let start = (t * hop) as isize - half as isize;
        for i in 0..size {
            let s = start + i as isize;
            let v = if s >= 0 && (s as usize) < samples.len() {
                samples[s as usize]
            } else {
                0.0
            };
            buf[i] = Complex::new(v * window[i], 0.0);
        }
        fft.process_with_scratch(&mut buf, &mut scratch);
        for b in 0..BANDS {
            let lo = edges[b];
            let hi = edges[b + 1].max(lo + 1).min(half);
            let lo = lo.min(hi - 1);
            let mean = buf[lo..hi].iter().map(|c| c.norm()).sum::<f32>() / (hi - lo) as f32;
            bands[t * BANDS + b] = mean;
            total += mean as f64;
        }
    }
    let mean = (total / bands.len() as f64) as f32;
    if mean <= f32::EPSILON {
        return None;
    }
    let reference = mean * 0.1;
    for value in &mut bands {
        *value = (1.0 + *value / reference).ln();
    }
    let mut flux = vec![0.0_f32; frames];
    let mut low = vec![0.0_f32; frames];
    for t in 1..frames {
        let (prev, cur) = (
            &bands[(t - 1) * BANDS..t * BANDS],
            &bands[t * BANDS..(t + 1) * BANDS],
        );
        for b in 0..BANDS {
            let rise = (cur[b] - prev[b]).max(0.0);
            flux[t] += rise;
            if b < low_bands {
                low[t] += rise;
            }
        }
    }
    let level = bands.iter().sum::<f32>() / bands.len() as f32;
    let mean_flux = flux.iter().sum::<f32>() / frames as f32;
    // A steady tone or silence has no rhythm to find.
    if level <= f32::EPSILON || mean_flux / (level * BANDS as f32) < 0.01 {
        return None;
    }
    let flux = normalise(&high_pass(&flux, fps))?;
    let low = normalise(&high_pass(&low, fps)).unwrap_or_else(|| vec![0.0; frames]);
    Some(Onsets { fps, flux, low })
}

/// Remove the local mean (~0.25 s either side) and keep the rises.
fn high_pass(signal: &[f32], fps: f32) -> Vec<f32> {
    let radius = ((fps * 0.25) as usize).max(1);
    let mut prefix = vec![0.0_f64; signal.len() + 1];
    for (i, v) in signal.iter().enumerate() {
        prefix[i + 1] = prefix[i] + *v as f64;
    }
    (0..signal.len())
        .map(|i| {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius + 1).min(signal.len());
            let local = ((prefix[hi] - prefix[lo]) / (hi - lo) as f64) as f32;
            (signal[i] - local).max(0.0)
        })
        .collect()
}

/// Scale to unit standard deviation; `None` for a flat signal.
fn normalise(signal: &[f32]) -> Option<Vec<f32>> {
    let n = signal.len() as f32;
    let mean = signal.iter().sum::<f32>() / n;
    let var = signal.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n;
    let sd = var.sqrt();
    (sd > f32::EPSILON).then(|| signal.iter().map(|v| v / sd).collect())
}

/// Analyse beats, meter, downbeats and tempo sections. `chroma` sharpens the
/// downbeat decision (harmony tends to change on the bar line); without it
/// the kick and accents decide alone.
pub fn analyze_rhythm(
    samples: &[f32],
    sample_rate: f32,
    chroma: Option<&ChromaFrames>,
    options: RhythmOptions,
) -> Option<RhythmAnalysis> {
    let onsets = onsets(samples, sample_rate)?;
    analyze_rhythm_from_onsets(&onsets, chroma, options)
}

pub fn analyze_rhythm_from_onsets(
    onsets: &Onsets,
    chroma: Option<&ChromaFrames>,
    options: RhythmOptions,
) -> Option<RhythmAnalysis> {
    let (min_bpm, max_bpm) = (
        options.min_bpm.clamp(20.0, 400.0),
        options.max_bpm.clamp(options.min_bpm.max(21.0), 500.0),
    );
    let fps = onsets.fps;
    let tempo = tempo_path(&onsets.flux, fps, min_bpm, max_bpm)?;
    let frames = track_beats(&onsets.flux, fps, &tempo.per_frame);
    if frames.len() < MIN_BEATS {
        return None;
    }
    let tracked: Vec<f64> = frames
        .iter()
        .map(|&f| refine_peak(&onsets.flux, f) / fps as f64)
        .collect();

    // Bars and sections on the tracked beats, then lock steady sections to
    // their grid and find the bars again on the locked beats.
    let (beats_per_bar, positions) = meter(onsets, &tracked, chroma, options);
    let first_pass = make_beats(onsets, &tracked, &positions);
    let rough = tempo_sections(&first_pass, beats_per_bar);
    let (seconds, fits) = lock_steady_sections(&tracked, &rough);
    let (beats_per_bar, beats) = if fits.iter().any(|f| f.steady) {
        let (beats_per_bar, positions) = meter(onsets, &seconds, chroma, options);
        (beats_per_bar, make_beats(onsets, &seconds, &positions))
    } else {
        (beats_per_bar, first_pass)
    };
    let strengths: Vec<f32> = beats.iter().map(|b| b.strength).collect();

    let tempo_curve = smoothed_tempo(&seconds);
    let sections = fits
        .iter()
        .enumerate()
        .map(|(i, fit)| {
            let end = fits.get(i + 1).map_or(seconds.len() - 1, |f| f.first_beat);
            TempoSection {
                start_seconds: seconds[fit.first_beat],
                end_seconds: seconds[end],
                bpm: fit.bpm,
                first_beat: fit.first_beat,
            }
        })
        .collect::<Vec<_>>();
    let total: f64 = sections
        .iter()
        .map(|s| s.end_seconds - s.start_seconds)
        .sum();
    let bpm = if total > 0.0 {
        (sections
            .iter()
            .map(|s| s.bpm as f64 * (s.end_seconds - s.start_seconds))
            .sum::<f64>()
            / total) as f32
    } else {
        tempo.global_bpm
    };
    let (lo, hi) = tempo_curve
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), (_, b)| {
            (lo.min(*b), hi.max(*b))
        });
    let variable = sections.len() > 1 || (hi - lo) / bpm.max(1.0) > 0.06;

    let mean_all = onsets.flux.iter().sum::<f32>() / onsets.flux.len() as f32;
    let mean_beats = strengths.iter().sum::<f32>() / strengths.len() as f32;
    let ratio = mean_beats / mean_all.max(1e-6);
    let confidence = (1.0 - (-(ratio - 1.0).max(0.0) / 2.0).exp()).clamp(0.0, 1.0);

    Some(RhythmAnalysis {
        beats,
        beats_per_bar,
        sections,
        bpm,
        variable,
        tempo_curve,
        confidence,
    })
}

/// Meter and bar position (0-based) per beat.
fn meter(
    onsets: &Onsets,
    seconds: &[f64],
    chroma: Option<&ChromaFrames>,
    options: RhythmOptions,
) -> (u32, Vec<u32>) {
    let frames = beat_frames(onsets, seconds);
    let evidence = downbeat_evidence(onsets, &frames, seconds, chroma);
    match options.beats_per_bar {
        Some(m) => {
            let m = m.clamp(1, 16);
            (m, bar_positions(&evidence, m).1)
        }
        None => {
            let (score3, pos3) = bar_positions(&evidence, 3);
            let (score4, pos4) = bar_positions(&evidence, 4);
            if score3 > score4 + FOUR_BIAS * evidence.len() as f32 {
                (3, pos3)
            } else {
                (4, pos4)
            }
        }
    }
}

fn beat_frames(onsets: &Onsets, seconds: &[f64]) -> Vec<usize> {
    let last = onsets.flux.len().saturating_sub(1);
    seconds
        .iter()
        .map(|s| ((s * onsets.fps as f64).round().max(0.0) as usize).min(last))
        .collect()
}

fn make_beats(onsets: &Onsets, seconds: &[f64], positions: &[u32]) -> Vec<Beat> {
    beat_frames(onsets, seconds)
        .iter()
        .zip(seconds)
        .zip(positions)
        .map(|((&f, &s), &p)| Beat {
            seconds: s,
            position: p + 1,
            strength: window_max(&onsets.flux, f, 3),
        })
        .collect()
}

/// One section after locking: where it starts in the final beat list, its
/// tempo, and whether its beats were locked to a constant grid.
#[derive(Clone, Copy, Debug)]
struct SectionFit {
    first_beat: usize,
    bpm: f32,
    steady: bool,
}

/// A constant beat grid `anchor + k * period`.
#[derive(Clone, Copy, Debug)]
struct Grid {
    period: f64,
    anchor: f64,
    /// Beats within [`GRID_TOLERANCE`] of a grid line or of the half-way
    /// point between two.
    consistent: usize,
}

/// Replace the beats of every section that one constant grid explains with
/// that grid. Returns the new beat times and one fit per section.
fn lock_steady_sections(tracked: &[f64], sections: &[TempoSection]) -> (Vec<f64>, Vec<SectionFit>) {
    // Half-open beat ranges per section.
    let mut ranges: Vec<(usize, usize, f32)> = sections
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let end = sections.get(i + 1).map_or(tracked.len(), |n| n.first_beat);
            (s.first_beat, end.max(s.first_beat + 1), s.bpm)
        })
        .collect();
    if ranges.is_empty() {
        ranges.push((0, tracked.len(), 0.0));
    }
    let steady_fit = |a: usize, b: usize| {
        fit_grid(&tracked[a..b]).filter(|g| g.consistent as f32 >= STEADY_SHARE * (b - a) as f32)
    };
    let mut fitted: Vec<(usize, usize, f32, Option<Grid>)> = ranges
        .iter()
        .map(|&(a, b, bpm)| (a, b, bpm, steady_fit(a, b)))
        .collect();
    // Neighbours at the same tempo are one section if one grid holds both
    // (a glitch can split a steady song).
    let mut i = 0;
    while i + 1 < fitted.len() {
        let (a, _, _, ga) = fitted[i];
        let (_, b, _, gb) = fitted[i + 1];
        if let (Some(ga), Some(gb)) = (ga, gb) {
            if (ga.period / gb.period - 1.0).abs() < MERGE_RATIO {
                if let Some(g) = steady_fit(a, b) {
                    fitted[i] = (a, b, (60.0 / g.period) as f32, Some(g));
                    fitted.remove(i + 1);
                    continue;
                }
            }
        }
        i += 1;
    }

    let mut out: Vec<f64> = Vec::with_capacity(tracked.len());
    let mut fits = Vec::with_capacity(fitted.len());
    for (a, b, bpm, grid) in fitted {
        let first_beat = out.len();
        let push = |out: &mut Vec<f64>, t: f64, period: f64| {
            if out.last().is_none_or(|&last| t - last > 0.5 * period) {
                out.push(t);
            }
        };
        match grid.map(|g| snap_grid(&tracked[a..b], g)) {
            Some(g) => {
                let k0 = ((tracked[a] - g.anchor) / g.period).round() as i64;
                let k1 = ((tracked[b - 1] - g.anchor) / g.period).round() as i64;
                for k in k0..=k1 {
                    push(&mut out, g.anchor + k as f64 * g.period, g.period);
                }
                fits.push(SectionFit {
                    first_beat,
                    bpm: (60.0 / g.period) as f32,
                    steady: true,
                });
            }
            None => {
                let period = median(tracked[a..b].windows(2).map(|w| w[1] - w[0]));
                for &t in &tracked[a..b] {
                    push(&mut out, t, period);
                }
                fits.push(SectionFit {
                    first_beat,
                    bpm,
                    steady: false,
                });
            }
        }
        // A section whose beats all fell to its neighbour is dropped.
        if out.len() == first_beat {
            fits.pop();
        }
    }
    (out, fits)
}

/// Robust constant-grid fit: indices from rounded intervals first (so a
/// slightly wrong period cannot slip a beat over a long section), then
/// least squares over the on-grid beats, repeated with indices from the fit.
fn fit_grid(times: &[f64]) -> Option<Grid> {
    if times.len() < MIN_BEATS {
        return None;
    }
    let mut ibis: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    ibis.sort_by(|a, b| a.total_cmp(b));
    let mut period = ibis[ibis.len() / 2];
    if period <= 1.0e-3 {
        return None;
    }
    let mut k: Vec<i64> = Vec::with_capacity(times.len());
    let mut acc = 0i64;
    k.push(0);
    for w in times.windows(2) {
        acc += ((w[1] - w[0]) / period).round().max(1.0) as i64;
        k.push(acc);
    }
    let mut anchor = median(times.iter().zip(&k).map(|(t, &k)| t - k as f64 * period));
    for pass in 0..4 {
        if pass > 0 {
            k = times
                .iter()
                .map(|t| ((t - anchor) / period).round() as i64)
                .collect();
        }
        let tol = GRID_TOLERANCE as f64 * period;
        let on: Vec<(f64, f64)> = times
            .iter()
            .zip(&k)
            .filter(|(t, k)| (*t - (anchor + **k as f64 * period)).abs() < tol)
            .map(|(t, &k)| (k as f64, *t))
            .collect();
        if on.len() < 2 {
            return None;
        }
        let n = on.len() as f64;
        let (kb, tb) = on
            .iter()
            .fold((0.0, 0.0), |(a, b), (k, t)| (a + k / n, b + t / n));
        let (sxy, sxx) = on.iter().fold((0.0, 0.0), |(xy, xx), (k, t)| {
            (xy + (k - kb) * (t - tb), xx + (k - kb) * (k - kb))
        });
        if sxx <= 0.0 {
            return None;
        }
        period = sxy / sxx;
        anchor = tb - period * kb;
        if period <= 1.0e-3 {
            return None;
        }
    }
    let (consistent, _) = count_on_grid(times, period, anchor);
    Some(Grid {
        period,
        anchor,
        consistent,
    })
}

/// The whole-BPM grid, re-anchored, when it keeps nearly every consistent
/// beat.
fn snap_grid(times: &[f64], grid: Grid) -> Grid {
    let bpm = (60.0 / grid.period).round();
    if bpm < 1.0 {
        return grid;
    }
    let period = 60.0 / bpm;
    let (_, anchor) = count_on_grid(times, period, grid.anchor);
    let (consistent, anchor) = count_on_grid(times, period, anchor);
    if consistent as f32 + SNAP_SLACK >= SNAP_KEEP * grid.consistent as f32 {
        Grid {
            period,
            anchor,
            consistent,
        }
    } else {
        grid
    }
}

/// Beats consistent with the grid (on a line or half-way between two), and
/// the anchor re-centred on the mean offset of the beats on a line.
fn count_on_grid(times: &[f64], period: f64, anchor: f64) -> (usize, f64) {
    let tol = GRID_TOLERANCE as f64 * period;
    let mut on = Vec::new();
    let mut half = 0usize;
    for t in times {
        let d = t - (anchor + ((t - anchor) / period).round() * period);
        if d.abs() < tol {
            on.push(d);
        } else if (d.abs() - 0.5 * period).abs() < tol {
            half += 1;
        }
    }
    let shift = if on.is_empty() {
        0.0
    } else {
        on.iter().sum::<f64>() / on.len() as f64
    };
    (on.len() + half, anchor + shift)
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut v: Vec<f64> = values.collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

struct TempoPath {
    /// BPM at every onset frame.
    per_frame: Vec<f32>,
    global_bpm: f32,
}

/// Local tempo by Viterbi over a windowed autocorrelation tempogram.
fn tempo_path(onset: &[f32], fps: f32, min_bpm: f32, max_bpm: f32) -> Option<TempoPath> {
    let n = onset.len();
    let window = ((TEMPO_WINDOW_SECONDS * fps) as usize).min(n).max(64);
    let hop = ((TEMPO_HOP_SECONDS * fps) as usize).max(1);
    let states: Vec<f32> = {
        let mut v = Vec::new();
        let mut b = min_bpm;
        while b <= max_bpm {
            v.push(b);
            b *= TEMPO_GRID_RATIO;
        }
        v
    };
    if states.len() < 2 {
        return None;
    }

    let mut centres = Vec::new();
    let mut salience: Vec<Vec<f32>> = Vec::new();
    let mut global = vec![0.0_f32; states.len()];
    let mut start = 0usize;
    loop {
        let end = (start + window).min(n);
        let slice = &onset[start..end];
        if let Some(acf) = autocorrelation(slice) {
            let s: Vec<f32> = states
                .iter()
                .map(|&bpm| comb_score(&acf, 60.0 * fps / bpm))
                .collect();
            for (g, v) in global.iter_mut().zip(&s) {
                *g += v;
            }
            centres.push((start + end) as f32 * 0.5);
            salience.push(s);
        }
        if end >= n {
            break;
        }
        start += hop;
    }
    if salience.is_empty() {
        return None;
    }
    // Whole-file tempo, octave-resolved by a log-normal prior at 120 BPM:
    // the anchor every window's choice is pulled toward.
    let global_idx = (0..states.len())
        .max_by(|&a, &b| {
            let pa = global[a] * log_normal(states[a], 120.0, 1.0);
            let pb = global[b] * log_normal(states[b], 120.0, 1.0);
            pa.total_cmp(&pb)
        })
        .unwrap_or(0);
    let global_bpm = states[global_idx];

    // Viterbi. Transitions only within a factor of two per hop.
    let ln_ratio = (TEMPO_GRID_RATIO).ln();
    let band = (2f32.ln() / ln_ratio) as isize;
    // Octave prior on every window: log-normal at 120 BPM, one octave wide.
    // Centring it on the whole-file tempo instead pulled the second half of a
    // 90 -> 140 song down to 70, the octave nearer the average.
    let prior: Vec<f32> = states
        .iter()
        .map(|&b| TEMPO_PRIOR * log_normal(b, 120.0, 1.0).ln())
        .collect();
    let emit = |s: &[f32]| -> Vec<f32> {
        let peak = s.iter().copied().fold(0.0_f32, f32::max).max(1e-6);
        s.iter()
            .map(|v| (v / peak).max(0.0).mul_add(1.0, 0.02).ln())
            .collect()
    };
    let k = states.len();
    let mut score: Vec<f32> = emit(&salience[0])
        .iter()
        .zip(&prior)
        .map(|(e, p)| e + p)
        .collect();
    let mut back: Vec<Vec<u32>> = Vec::with_capacity(salience.len());
    back.push(vec![0; k]);
    for s in salience.iter().skip(1) {
        let e = emit(s);
        let mut next = vec![f32::NEG_INFINITY; k];
        let mut from = vec![0u32; k];
        for j in 0..k {
            let lo = (j as isize - band).max(0) as usize;
            let hi = ((j as isize + band) as usize).min(k - 1);
            let mut best = f32::NEG_INFINITY;
            let mut arg = j;
            for i in lo..=hi {
                let d = (i as f32 - j as f32) * ln_ratio;
                let v = score[i] - TEMPO_TRANSITION * d * d;
                if v > best {
                    best = v;
                    arg = i;
                }
            }
            next[j] = best + e[j] + prior[j];
            from[j] = arg as u32;
        }
        score = next;
        back.push(from);
    }
    let mut state = (0..k)
        .max_by(|&a, &b| score[a].total_cmp(&score[b]))
        .unwrap_or(0);
    let mut path = vec![0usize; salience.len()];
    for w in (0..salience.len()).rev() {
        path[w] = state;
        state = back[w][state] as usize;
    }

    // Per-frame tempo: interpolate log-BPM between window centres.
    let mut per_frame = vec![0.0_f32; n];
    for (t, value) in per_frame.iter_mut().enumerate() {
        let x = t as f32;
        let w = centres.partition_point(|&c| c <= x);
        let bpm = if w == 0 {
            states[path[0]]
        } else if w >= centres.len() {
            states[path[centres.len() - 1]]
        } else {
            let (c0, c1) = (centres[w - 1], centres[w]);
            let f = ((x - c0) / (c1 - c0).max(1e-6)).clamp(0.0, 1.0);
            let (a, b) = (states[path[w - 1]].ln(), states[path[w]].ln());
            (a + (b - a) * f).exp()
        };
        *value = bpm;
    }
    Some(TempoPath {
        per_frame,
        global_bpm,
    })
}

fn log_normal(bpm: f32, centre: f32, octaves: f32) -> f32 {
    let x = (bpm / centre).log2() / octaves;
    (-0.5 * x * x).exp()
}

/// Comb over period multiples with linear interpolation on the ACF.
fn comb_score(acf: &[f32], period: f32) -> f32 {
    let mut s = 0.0;
    for (k, w) in COMB.iter().enumerate() {
        let lag = period * (k + 1) as f32;
        let i = lag.floor() as usize;
        if i + 1 >= acf.len() {
            break;
        }
        let f = lag - i as f32;
        s += w * (acf[i] * (1.0 - f) + acf[i + 1] * f);
    }
    s.max(0.0)
}

/// Normalised autocorrelation (lag 0 = 1) of a mean-removed signal, via FFT.
fn autocorrelation(signal: &[f32]) -> Option<Vec<f32>> {
    let n = signal.len();
    if n < 8 {
        return None;
    }
    let mean = signal.iter().sum::<f32>() / n as f32;
    let size = (2 * n).next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(size);
    let inverse = planner.plan_fft_inverse(size);
    let mut buf: Vec<Complex<f32>> = signal
        .iter()
        .map(|&v| Complex::new(v - mean, 0.0))
        .chain(std::iter::repeat_n(Complex::new(0.0, 0.0), size - n))
        .collect();
    forward.process(&mut buf);
    for value in &mut buf {
        *value = Complex::new(value.norm_sqr(), 0.0);
    }
    inverse.process(&mut buf);
    let zero = buf[0].re;
    (zero > f32::EPSILON).then(|| buf[..n].iter().map(|c| c.re / zero).collect())
}

/// Dynamic-programming beat tracker with a time-varying period.
fn track_beats(onset: &[f32], fps: f32, bpm: &[f32]) -> Vec<usize> {
    let n = onset.len();
    // Light smoothing so the score rewards landing *near* an onset.
    let local: Vec<f32> = (0..n)
        .map(|t| {
            let a = t.saturating_sub(2);
            let b = (t + 3).min(n);
            let weights = [0.25, 0.6, 1.0, 0.6, 0.25];
            let mut s = 0.0;
            for (k, i) in (a..b).enumerate() {
                s += onset[i] * weights[k + (2 - (t - a))];
            }
            s
        })
        .collect();
    let mut cum = vec![0.0_f32; n];
    let mut back = vec![usize::MAX; n];
    for t in 0..n {
        let period = 60.0 * fps / bpm[t].max(1.0);
        let lo = t as isize - (2.0 * period).round() as isize;
        let hi = t as isize - (0.5 * period).round() as isize;
        let mut best = 0.0_f32;
        let mut arg = usize::MAX;
        if hi >= 0 {
            for tau in lo.max(0) as usize..=hi as usize {
                let ratio = ((t - tau) as f32 / period).ln();
                let v = cum[tau] - TIGHTNESS * ratio * ratio;
                if arg == usize::MAX || v > best {
                    best = v;
                    arg = tau;
                }
            }
        }
        // A chain that only loses score is worse than starting fresh here.
        if arg != usize::MAX && best > 0.0 {
            cum[t] = local[t] + best;
            back[t] = arg;
        } else {
            cum[t] = local[t];
        }
    }
    // End on the best-scoring frame within the last period.
    let last_period = (60.0 * fps / bpm[n - 1].max(1.0)) as usize;
    let tail = n.saturating_sub(last_period.max(1));
    let mut t = (tail..n)
        .max_by(|&a, &b| cum[a].total_cmp(&cum[b]))
        .unwrap_or(n - 1);
    let mut beats = vec![t];
    while back[t] != usize::MAX {
        t = back[t];
        beats.push(t);
    }
    beats.reverse();
    trim_weak_edges(&beats, onset)
}

/// Drop leading and trailing beats that sit in silence (count-in rests, fade
/// tails): a beat grid there is invented, not heard.
fn trim_weak_edges(beats: &[usize], onset: &[f32]) -> Vec<usize> {
    if beats.len() < 4 {
        return beats.to_vec();
    }
    let strength: Vec<f32> = beats.iter().map(|&b| window_max(onset, b, 4)).collect();
    let mut sorted = strength.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2];
    let threshold = median * 0.25;
    // A run of weak beats at an edge is silence; a single weak beat inside
    // music (a rest) is kept.
    let first = strength.iter().position(|&s| s >= threshold).unwrap_or(0);
    let last = strength
        .iter()
        .rposition(|&s| s >= threshold)
        .unwrap_or(beats.len() - 1);
    beats[first..=last].to_vec()
}

fn window_max(signal: &[f32], centre: usize, radius: usize) -> f32 {
    let a = centre.saturating_sub(radius);
    let b = (centre + radius + 1).min(signal.len());
    signal[a..b].iter().copied().fold(0.0_f32, f32::max)
}

/// Sub-frame position of the onset peak nearest `frame`.
fn refine_peak(onset: &[f32], frame: usize) -> f64 {
    let n = onset.len();
    let a = frame.saturating_sub(2);
    let b = (frame + 3).min(n);
    let peak = (a..b)
        .max_by(|&x, &y| onset[x].total_cmp(&onset[y]))
        .unwrap_or(frame);
    if peak == 0 || peak + 1 >= n || onset[peak] <= 0.0 {
        return frame as f64;
    }
    let (l, c, r) = (onset[peak - 1], onset[peak], onset[peak + 1]);
    let denom = l - 2.0 * c + r;
    let offset = if denom.abs() > 1e-9 {
        (0.5 * (l - r) / denom).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    peak as f64 + offset as f64
}

/// Per-beat evidence that the beat starts a bar, z-scored.
fn downbeat_evidence(
    onsets: &Onsets,
    frames: &[usize],
    seconds: &[f64],
    chroma: Option<&ChromaFrames>,
) -> Vec<f32> {
    let n = frames.len();
    let onset: Vec<f32> = frames
        .iter()
        .map(|&f| window_max(&onsets.flux, f, 3))
        .collect();
    let low: Vec<f32> = frames
        .iter()
        .map(|&f| window_max(&onsets.low, f, 4))
        .collect();
    let ctx = HARMONY_CONTEXT;
    let harmonic: Vec<f32> = match chroma {
        Some(chroma) if !chroma.treble.is_empty() => {
            // Chroma over the beats before vs the beats after.
            let at = |i: isize| seconds[i.clamp(0, n as isize - 1) as usize];
            (0..n as isize)
                .map(|i| {
                    if i < ctx || i + ctx >= n as isize {
                        return 0.0;
                    }
                    let (before, _, _) = chroma.span(at(i - ctx), at(i));
                    let (after, _, _) = chroma.span(at(i), at(i + ctx));
                    1.0 - cosine(&before, &after)
                })
                .collect()
        }
        _ => vec![0.0; n],
    };
    let (z_onset, z_low, z_harm) = (zscore(&onset), zscore(&low), zscore(&harmonic));
    (0..n)
        .map(|i| W_ACCENT * z_onset[i] + W_KICK * z_low[i] + W_HARMONY * z_harm[i])
        .collect()
}

fn cosine(a: &[f32; 12], b: &[f32; 12]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return 1.0;
    }
    dot / (na * nb)
}

fn zscore(values: &[f32]) -> Vec<f32> {
    let n = values.len().max(1) as f32;
    let mean = values.iter().sum::<f32>() / n;
    let sd = (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n).sqrt();
    if sd <= f32::EPSILON {
        return vec![0.0; values.len()];
    }
    values.iter().map(|v| (v - mean) / sd).collect()
}

/// Viterbi over bar positions `0..m` (0 = downbeat): `(score, position per beat)`.
fn bar_positions(evidence: &[f32], m: u32) -> (f32, Vec<u32>) {
    let m = m.max(1) as usize;
    let n = evidence.len();
    if m == 1 {
        return (0.0, vec![0; n]);
    }
    let reset = BAR_RESET;
    let stay = (1.0 - reset).ln();
    let jump = (reset / m as f32).ln();
    let emit = |i: usize, pos: usize| {
        if pos == 0 {
            evidence[i]
        } else {
            -evidence[i] / (m - 1) as f32
        }
    };
    let mut score: Vec<f32> = (0..m).map(|p| emit(0, p)).collect();
    let mut back = vec![vec![0u8; m]; n];
    for i in 1..n {
        let mut next = vec![f32::NEG_INFINITY; m];
        for (to, slot) in next.iter_mut().enumerate() {
            let mut best = f32::NEG_INFINITY;
            let mut arg = 0;
            for (from, &s) in score.iter().enumerate() {
                let t = if (from + 1) % m == to { stay } else { jump };
                if s + t > best {
                    best = s + t;
                    arg = from;
                }
            }
            *slot = best + emit(i, to);
            back[i][to] = arg as u8;
        }
        score = next;
    }
    let mut state = (0..m)
        .max_by(|&a, &b| score[a].total_cmp(&score[b]))
        .unwrap_or(0);
    let best = score[state];
    let mut path = vec![0u32; n];
    for i in (0..n).rev() {
        path[i] = state as u32;
        state = back[i][state] as usize;
    }
    (best, path)
}

/// Local tempo per beat from a 5-beat median of inter-beat intervals.
fn smoothed_tempo(seconds: &[f64]) -> Vec<(f64, f32)> {
    let ibis: Vec<f64> = seconds.windows(2).map(|w| w[1] - w[0]).collect();
    (0..seconds.len())
        .map(|i| {
            let a = i.saturating_sub(2);
            let b = (i + 3).min(ibis.len());
            let mut window: Vec<f64> = ibis[a.min(b.saturating_sub(1))..b].to_vec();
            window.sort_by(|x, y| x.total_cmp(y));
            let ibi = window.get(window.len() / 2).copied().unwrap_or(0.5);
            (seconds[i], (60.0 / ibi.max(1e-3)) as f32)
        })
        .collect()
}

/// Piecewise-constant tempo over bars by optimal change-point search.
fn tempo_sections(beats: &[Beat], beats_per_bar: u32) -> Vec<TempoSection> {
    // Bar spans: downbeat to downbeat, with the partial edges as their own
    // spans so every beat belongs somewhere.
    let mut marks: Vec<usize> = beats
        .iter()
        .enumerate()
        .filter(|(_, b)| b.position == 1)
        .map(|(i, _)| i)
        .collect();
    if marks.first() != Some(&0) {
        marks.insert(0, 0);
    }
    if marks.last() != Some(&(beats.len() - 1)) {
        marks.push(beats.len() - 1);
    }
    let spans: Vec<(usize, usize)> = marks
        .windows(2)
        .filter(|w| w[1] > w[0])
        .map(|w| (w[0], w[1]))
        .collect();
    if spans.is_empty() {
        return Vec::new();
    }
    // ln(BPM) of each span and its weight (beats it covers).
    let values: Vec<(f32, f32)> = spans
        .iter()
        .map(|&(a, b)| {
            let dur = (beats[b].seconds - beats[a].seconds).max(1e-3);
            let bpm = 60.0 * (b - a) as f64 / dur;
            ((bpm as f32).ln(), (b - a) as f32)
        })
        .collect();
    let n = values.len();
    // Prefix sums for weighted SSE of a run.
    let mut w = vec![0.0_f64; n + 1];
    let mut wx = vec![0.0_f64; n + 1];
    let mut wxx = vec![0.0_f64; n + 1];
    for (i, &(x, weight)) in values.iter().enumerate() {
        let (x, weight) = (x as f64, weight as f64);
        w[i + 1] = w[i] + weight;
        wx[i + 1] = wx[i] + weight * x;
        wxx[i + 1] = wxx[i] + weight * x * x;
    }
    let cost = |a: usize, b: usize| {
        let sw = w[b] - w[a];
        let sx = wx[b] - wx[a];
        (wxx[b] - wxx[a] - sx * sx / sw.max(1e-9)) as f32
    };
    let penalty = SECTION_PENALTY * beats_per_bar.max(1) as f32 * 8.0;
    let mut best = vec![f32::INFINITY; n + 1];
    let mut from = vec![0usize; n + 1];
    best[0] = 0.0;
    for b in 1..=n {
        for a in 0..b {
            let v = best[a] + cost(a, b) + penalty;
            if v < best[b] {
                best[b] = v;
                from[b] = a;
            }
        }
    }
    let mut cuts = vec![n];
    let mut b = n;
    while b > 0 {
        b = from[b];
        cuts.push(b);
    }
    cuts.reverse();
    cuts.windows(2)
        .map(|c| {
            let (first_span, last_span) = (c[0], c[1] - 1);
            let first_beat = spans[first_span].0;
            let last_beat = spans[last_span].1;
            let dur = (beats[last_beat].seconds - beats[first_beat].seconds).max(1e-3);
            TempoSection {
                start_seconds: beats[first_beat].seconds,
                end_seconds: beats[last_beat].seconds,
                bpm: (60.0 * (last_beat - first_beat) as f64 / dur) as f32,
                first_beat,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 22_050.0;

    /// A drum pattern over beat times: kick on beat 1 of each bar, snare on
    /// 2 and 4 (or 2 and 3 in 3/4), hats on every beat, plus a chord that
    /// changes every bar.
    fn render(beats: &[f64], per_bar: usize, seconds: f64) -> Vec<f32> {
        let n = (seconds * SR as f64) as usize;
        let mut out = vec![0.0_f32; n];
        let mut seed = 1u32;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let roots = [130.81_f32, 174.61, 196.0, 110.0];
        for (i, &t) in beats.iter().enumerate() {
            let start = (t * SR as f64) as usize;
            let pos = i % per_bar;
            for k in 0..(0.25 * SR) as usize {
                let s = start + k;
                if s >= n {
                    break;
                }
                let tt = k as f32 / SR;
                let env = (-tt * 18.0).exp();
                let mut v = 0.15 * noise() * (-tt * 60.0).exp(); // hat
                if pos == 0 {
                    v += 0.9 * (std::f32::consts::TAU * 55.0 * tt).sin() * env; // kick
                } else if pos % 2 == 1 {
                    v += 0.35 * noise() * (-tt * 25.0).exp(); // snare
                }
                out[s] += v;
            }
            if pos == 0 {
                let root = roots[(i / per_bar) % roots.len()];
                let end = beats
                    .get(i + per_bar)
                    .map(|b| (b * SR as f64) as usize)
                    .unwrap_or(n);
                for s in start..end.min(n) {
                    let tt = s as f32 / SR;
                    for mult in [1.0_f32, 1.26, 1.5] {
                        out[s] += 0.05 * (std::f32::consts::TAU * root * 2.0 * mult * tt).sin();
                    }
                }
            }
        }
        out
    }

    fn steady(bpm: f64, start: f64, seconds: f64) -> Vec<f64> {
        let period = 60.0 / bpm;
        let mut t = start;
        let mut v = Vec::new();
        while t < seconds - 0.3 {
            v.push(t);
            t += period;
        }
        v
    }

    fn analyze(audio: &[f32], opts: RhythmOptions) -> RhythmAnalysis {
        let chroma = super::super::chroma::chroma_frames(audio, SR);
        analyze_rhythm(audio, SR, chroma.as_ref(), opts).expect("rhythm")
    }

    /// Share of reference beats matched within ±70 ms.
    fn recall(found: &[f64], truth: &[f64]) -> f64 {
        let hits = truth
            .iter()
            .filter(|t| found.iter().any(|f| (f - *t).abs() <= 0.07))
            .count();
        hits as f64 / truth.len() as f64
    }

    #[test]
    fn a_steady_groove_gets_its_beats_bars_and_tempo() {
        let truth = steady(128.0, 0.5, 20.0);
        let audio = render(&truth, 4, 20.0);
        let r = analyze(&audio, RhythmOptions::default());
        assert!((r.bpm - 128.0).abs() < 1.0, "{}", r.bpm);
        assert!(!r.variable, "{:?}", r.sections);
        let found: Vec<f64> = r.beats.iter().map(|b| b.seconds).collect();
        assert!(recall(&found, &truth) > 0.95);
        assert_eq!(r.beats_per_bar, 4);
        let downbeats: Vec<f64> = truth.iter().step_by(4).copied().collect();
        assert!(
            recall(&r.downbeats(), &downbeats) > 0.9,
            "{:?}",
            &r.downbeats()[..4]
        );
    }

    #[test]
    fn a_click_track_with_sloppy_hits_locks_to_one_whole_tempo() {
        // 120 BPM played ±8 ms around the click.
        let truth = steady(120.0, 0.5, 30.0);
        let played: Vec<f64> = truth
            .iter()
            .enumerate()
            .map(|(i, t)| t + 0.008 * (((i * 7919) % 17) as f64 / 8.0 - 1.0))
            .collect();
        let audio = render(&played, 4, 30.0);
        let r = analyze(&audio, RhythmOptions::default());
        assert_eq!(r.sections.len(), 1, "{:?}", r.sections);
        assert_eq!(r.sections[0].bpm, 120.0);
        assert!(!r.variable);
        let found: Vec<f64> = r.beats.iter().map(|b| b.seconds).collect();
        assert!(
            found.windows(2).all(|w| (w[1] - w[0] - 0.5).abs() < 1e-9),
            "beats are not on one grid"
        );
        assert!(recall(&found, &truth) > 0.95);
    }

    #[test]
    fn stretches_tracked_on_the_off_beat_still_lock_to_the_song_grid() {
        // 120 BPM from 1.0 s; the tracker follows an off-beat kick for three
        // stretches of 12 beats (a third of the song).
        let tracked: Vec<f64> = (0..108)
            .map(|i| {
                let off = matches!(i % 36, 20..=31);
                1.0 + i as f64 * 0.5 + if off { 0.25 } else { 0.0 }
            })
            .collect();
        let section = TempoSection {
            start_seconds: tracked[0],
            end_seconds: tracked[107],
            bpm: 119.4,
            first_beat: 0,
        };
        let (locked, fits) = lock_steady_sections(&tracked, &[section]);
        assert_eq!(fits.len(), 1);
        assert!(fits[0].steady);
        assert_eq!(fits[0].bpm, 120.0);
        assert_eq!(locked.len(), 108);
        for (i, t) in locked.iter().enumerate() {
            assert!((t - (1.0 + i as f64 * 0.5)).abs() < 1e-9, "beat {i}: {t}");
        }
    }

    #[test]
    fn a_waltz_reads_as_three_beats_to_the_bar() {
        let truth = steady(150.0, 0.3, 24.0);
        let audio = render(&truth, 3, 24.0);
        let r = analyze(&audio, RhythmOptions::default());
        assert_eq!(r.beats_per_bar, 3);
        let downbeats: Vec<f64> = truth.iter().step_by(3).copied().collect();
        assert!(recall(&r.downbeats(), &downbeats) > 0.85);
    }

    #[test]
    fn a_song_that_changes_tempo_gets_two_sections() {
        // 90 BPM for 16 bars, then 140 BPM.
        let mut truth = steady(90.0, 0.4, 0.4 + 64.0 * 60.0 / 90.0 + 0.3);
        let switch = truth.last().unwrap() + 60.0 / 90.0;
        let tail = steady(140.0, switch, switch + 64.0 * 60.0 / 140.0 + 0.3);
        truth.extend(tail);
        let seconds = truth.last().unwrap() + 1.0;
        let audio = render(&truth, 4, seconds);
        let r = analyze(&audio, RhythmOptions::default());
        assert!(r.variable);
        assert!(r.sections.len() >= 2, "{:?}", r.sections);
        let first = r.sections.first().unwrap();
        let last = r.sections.last().unwrap();
        // Both sections are steady, so each locks to its whole tempo.
        assert_eq!(first.bpm, 90.0, "{:?}", r.sections);
        assert_eq!(last.bpm, 140.0, "{:?}", r.sections);
        let found: Vec<f64> = r.beats.iter().map(|b| b.seconds).collect();
        assert!(recall(&found, &truth) > 0.9, "{}", recall(&found, &truth));
    }

    #[test]
    fn a_tempo_ramp_is_followed_beat_by_beat() {
        // Accelerando 100 -> 130 BPM over 40 beats.
        let mut truth = Vec::new();
        let mut t = 0.5;
        for i in 0..72 {
            truth.push(t);
            let bpm = 100.0 + 30.0 * (i as f64 / 71.0);
            t += 60.0 / bpm;
        }
        let seconds = t + 0.5;
        let audio = render(&truth, 4, seconds);
        let r = analyze(&audio, RhythmOptions::default());
        let found: Vec<f64> = r.beats.iter().map(|b| b.seconds).collect();
        assert!(recall(&found, &truth) > 0.9, "{}", recall(&found, &truth));
        assert!(r.variable);
    }

    #[test]
    fn silence_and_a_steady_tone_have_no_rhythm() {
        assert!(
            analyze_rhythm(&vec![0.0; 22_050 * 5], SR, None, RhythmOptions::default()).is_none()
        );
        let tone: Vec<f32> = (0..22_050 * 5)
            .map(|i| (i as f32 * 0.05).sin() * 0.3)
            .collect();
        assert!(analyze_rhythm(&tone, SR, None, RhythmOptions::default()).is_none());
    }
}

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
//!    tracked beats. Each grid covers only the beats that sit on it, so a
//!    ritardando or push into the next section is tracked again beat by
//!    beat, with the tempo gliding between the two sections' tempos. The
//!    transition also settles the new section's octave: a ritardando leads
//!    into a slower tempo, so a slow section whose drums read at double
//!    speed is taken at half.
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
/// Beat DP tightness inside a transition between two steady sections: loose
/// enough to follow a ritardando or a push beat by beat.
const BRIDGE_TIGHTNESS: f32 = 30.0;
/// Transition intervals stay within these shares of the shorter and longer
/// neighbouring periods (no reading a fill's 8th notes as the beat).
const BRIDGE_SHORTEST: f64 = 0.7;
const BRIDGE_LONGEST: f64 = 1.5;
/// A transition played at one of its neighbours' tempos is taken as that
/// tempo when its beats hit onsets at least this well relative to the free
/// track.
const BRIDGE_PREFER_CONSTANT: f32 = 0.8;
/// A transition beat this close (share of a period) to a neighbouring
/// section's grid is that grid's beat.
const GRID_SNAP: f64 = 0.03;
/// A transition "heads somewhere" when its intervals all move one way, each
/// step allowed this much wobble, by at least [`TREND_MIN`] overall.
const TREND_WOBBLE: f64 = 0.03;
const TREND_MIN: f64 = 0.1;
/// Steps (beyond the wobble) a transition must take its way: one jump is a
/// tempo change, not a ritardando.
const TREND_STEPS: usize = 2;
/// Mean strength of a transition's off-grid beats, relative to the previous
/// section's typical beat, for its direction to count.
const TREND_STRENGTH: f32 = 0.7;
/// Bars before a section change that are tracked again as a possible
/// transition (a grid can run on past where the tempo starts to move).
const TRANSITION_LOOKBACK_BARS: usize = 2;

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
    /// On a steady section's constant grid. Off-grid beats next to grid ones
    /// are a transition (a ritardando, a push, a fill into a new section).
    pub locked: bool,
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
    /// Local tempo per beat: `(seconds, bpm)`, for display. Beat by beat once
    /// the song has steady sections (so a transition shows each of its
    /// tempos); smoothed over 5 beats for a freely played one.
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
    let (seconds, fits) = lock_steady_sections(onsets, &tracked, &rough, beats_per_bar);
    let (beats_per_bar, beats) = if fits.iter().any(|f| f.grid.is_some()) {
        let (beats_per_bar, positions) = meter(onsets, &seconds, chroma, options);
        let mut beats = make_beats(onsets, &seconds, &positions);
        mark_locked(&mut beats, &fits);
        (beats_per_bar, beats)
    } else {
        (beats_per_bar, first_pass)
    };
    let strengths: Vec<f32> = beats.iter().map(|b| b.strength).collect();

    let smoothed = smoothed_tempo(&seconds);
    let tempo_curve = if beats.iter().any(|b| b.locked) {
        per_beat_tempo(&seconds)
    } else {
        smoothed.clone()
    };
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
    let (lo, hi) = smoothed
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
            locked: false,
        })
        .collect()
}

/// Mark the beats that sit exactly on their section's grid.
fn mark_locked(beats: &mut [Beat], fits: &[SectionFit]) {
    for (i, fit) in fits.iter().enumerate() {
        let Some(g) = fit.grid else {
            continue;
        };
        let end = fits
            .get(i + 1)
            .map_or(beats.len(), |f| f.first_beat)
            .min(beats.len());
        for beat in &mut beats[fit.first_beat..end] {
            let k = ((beat.seconds - g.anchor) / g.period).round();
            beat.locked = (beat.seconds - (g.anchor + k * g.period)).abs() < 1.0e-6;
        }
    }
}

/// One section after locking: where it starts in the final beat list, its
/// tempo, and the constant grid its beats were locked to, if any.
#[derive(Clone, Copy, Debug)]
struct SectionFit {
    first_beat: usize,
    bpm: f32,
    grid: Option<Grid>,
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
fn lock_steady_sections(
    onsets: &Onsets,
    tracked: &[f64],
    sections: &[TempoSection],
    beats_per_bar: u32,
) -> (Vec<f64>, Vec<SectionFit>) {
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

    // Each section's own beats: a steady one's grid over the span of beats
    // that sit on it, or the tracked beats.
    let mut pieces: Vec<(Vec<f64>, f32, Option<Grid>)> = fitted
        .into_iter()
        .map(|(a, b, bpm, grid)| {
            let times = &tracked[a..b];
            match grid.map(|g| snap_grid(times, g)) {
                Some(g) => {
                    let on: Vec<i64> = times
                        .iter()
                        .filter(|&&t| consistent_with(t, g.period, g.anchor))
                        .map(|t| ((t - g.anchor) / g.period).round() as i64)
                        .collect();
                    let (k0, k1) = (on[0], on[on.len() - 1]);
                    let beats = (k0..=k1).map(|k| g.anchor + k as f64 * g.period).collect();
                    (beats, (60.0 / g.period) as f32, Some(g))
                }
                None => (times.to_vec(), bpm, None),
            }
        })
        .collect();

    let mut out: Vec<f64> = Vec::with_capacity(tracked.len());
    let mut fits = Vec::with_capacity(pieces.len());
    for i in 0..pieces.len() {
        // Entering a steady section from another: re-track the last bar
        // before it (a ritardando starts inside it), and read the new
        // section at the octave the transition leads into.
        let prev_grid = i.checked_sub(1).and_then(|p| pieces[p].2);
        if let (Some(prev), Some(cur), Some(fit)) = (prev_grid, pieces[i].2, fits.last()) {
            let fit: &SectionFit = fit;
            // How hard the previous section's beats hit: a transition has
            // to be heard at least about as clearly to count.
            let recent = &out[fit.first_beat.max(out.len().saturating_sub(16))..];
            let typical = median(
                beat_frames(onsets, recent)
                    .iter()
                    .map(|&f| window_max(&onsets.flux, f, 2) as f64),
            ) as f32;
            let lookback = TRANSITION_LOOKBACK_BARS * beats_per_bar.max(1) as usize;
            let from = out
                .len()
                .saturating_sub(1 + lookback)
                .max(fit.first_beat + 1)
                .min(out.len().saturating_sub(1));
            out.truncate(from + 1);
            let (mut beats, grid, mut transition) =
                enter_section(onsets, out[from], prev, &pieces[i].0, cur, typical);
            // Transition beats already on the new grid are the section's own.
            let settled = transition
                .iter()
                .rev()
                .take_while(|&&t| on_grid(t, grid))
                .count();
            let early = transition.split_off(transition.len() - settled);
            beats.splice(0..0, early);
            out.extend(transition);
            pieces[i] = (beats, (60.0 / grid.period) as f32, Some(grid));
        }
        let (beats, bpm, grid) = &pieces[i];
        let Some(&start) = beats.first() else {
            continue;
        };
        let period = grid.map_or_else(
            || median(beats.windows(2).map(|w| w[1] - w[0])),
            |g| g.period,
        );
        match (
            out.last().copied(),
            i.checked_sub(1).and_then(|p| pieces[p].2),
        ) {
            // Between two steady sections: bridged above.
            (Some(_), Some(_)) if grid.is_some() => {}
            // Otherwise the tracked beats in the gap, if any.
            (Some(from), _) => {
                let gap = tracked
                    .iter()
                    .copied()
                    .filter(|&t| t > from + 0.5 * period && t < start - 0.5 * period);
                out.extend(gap);
            }
            // Before a steady first section: its grid carried back to the
            // first beat heard (a loose start is part of the intro).
            (None, _) if grid.is_some() => {
                let k = ((start - tracked[0]) / period).round().max(0.0) as i64;
                out.extend(
                    (1..=k)
                        .rev()
                        .map(|k| start - k as f64 * period)
                        .filter(|&t| t >= 0.0),
                );
            }
            (None, _) => {}
        }
        let first_beat = if fits.is_empty() { 0 } else { out.len() };
        for &t in beats {
            if out.last().is_none_or(|&last| t - last > 0.5 * period) {
                out.push(t);
            }
        }
        if out.len() > first_beat {
            fits.push(SectionFit {
                first_beat,
                bpm: *bpm,
                grid: *grid,
            });
        }
    }
    // After a steady last section: tracked beats past its grid.
    if let (Some(&last), Some((_, _, Some(g)))) = (out.last(), pieces.last()) {
        out.extend(
            tracked
                .iter()
                .copied()
                .filter(|&t| t > last + 0.5 * g.period),
        );
    }
    (out, fits)
}

/// Enter steady section `cur` (its grid beats `beats`) from the beat at
/// `from` on grid `prev`. Returns the section's beats and grid — at its
/// detected tempo, or at half or double it when the transition says so —
/// and the transition's beats in between.
///
/// Half and double tempo are the same drums read at another beat level, so
/// the audio of the section alone cannot choose. The transition can: a
/// ritardando leads into a slower tempo and an accelerando into a faster
/// one. When the detected tempo contradicts the direction the transition
/// is heading and the other octave continues it, the other octave wins.
fn enter_section(
    onsets: &Onsets,
    from: f64,
    prev: Grid,
    beats: &[f64],
    cur: Grid,
    typical: f32,
) -> (Vec<f64>, Grid, Vec<f64>) {
    let with_grid = |beats: Vec<f64>, period: f64| {
        let anchor = beats[0];
        (
            beats,
            Grid {
                period,
                anchor,
                consistent: cur.consistent,
            },
        )
    };
    let mut candidates = vec![(beats.to_vec(), cur)];
    for phase in 0..2 {
        let half: Vec<f64> = beats.iter().copied().skip(phase).step_by(2).collect();
        if half.len() >= 4 && 60.0 / (2.0 * cur.period) >= 30.0 {
            candidates.push(with_grid(half, 2.0 * cur.period));
        }
    }
    if 60.0 / (0.5 * cur.period) <= 300.0 {
        let double: Vec<f64> = beats
            .windows(2)
            .flat_map(|w| [w[0], 0.5 * (w[0] + w[1])])
            .chain(beats.last().copied())
            .collect();
        candidates.push(with_grid(double, 0.5 * cur.period));
    }

    let evaluated: Vec<(Vec<f64>, Grid, Vec<f64>, f64, Trend)> = candidates
        .into_iter()
        .map(|(beats, grid)| {
            let transition = snap_to_grids(
                bridge(onsets, from, beats[0], prev.period, grid.period),
                prev,
                grid,
            );
            let mut times = vec![from - prev.period, from];
            times.extend(&transition);
            times.extend([beats[0], beats[0] + grid.period]);
            let intervals: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
            let roughness = intervals
                .windows(2)
                .map(|w| (w[1] / w[0]).ln().powi(2))
                .sum::<f64>();
            // Only a transition played on real hits heads anywhere.
            let off_grid: Vec<f64> = transition
                .iter()
                .copied()
                .filter(|&t| !on_grid(t, prev) && !on_grid(t, grid))
                .collect();
            let heard = beat_frames(onsets, &off_grid)
                .iter()
                .map(|&f| window_max(&onsets.flux, f, 2))
                .sum::<f32>()
                / off_grid.len().max(1) as f32;
            let trend = if heard >= TREND_STRENGTH * typical {
                trend(&intervals[1..intervals.len() - 1])
            } else {
                Trend::Steady
            };
            (beats, grid, transition, roughness, trend)
        })
        .collect();
    let detected = cur.period;
    let continues = |grid: &Grid, trend: Trend| match trend {
        Trend::Slowing => grid.period > prev.period && detected < prev.period,
        Trend::Quickening => grid.period < prev.period && detected > prev.period,
        Trend::Steady => false,
    };
    let best = (1..evaluated.len())
        .filter(|&i| continues(&evaluated[i].1, evaluated[i].4))
        .min_by(|&a, &b| evaluated[a].3.total_cmp(&evaluated[b].3))
        .unwrap_or(0);
    let (beats, grid, transition, _, _) = evaluated.into_iter().nth(best).unwrap();
    (beats, grid, transition)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Trend {
    Steady,
    Slowing,
    Quickening,
}

/// Direction of a run of beat intervals: every step the same way (allowing
/// [`TREND_WOBBLE`]), at least [`TREND_STEPS`] real steps, and at least
/// [`TREND_MIN`] overall.
fn trend(intervals: &[f64]) -> Trend {
    let (Some(&first), Some(&last)) = (intervals.first(), intervals.last()) else {
        return Trend::Steady;
    };
    if intervals.len() < 3 {
        return Trend::Steady;
    }
    let slowing = intervals
        .windows(2)
        .all(|w| w[1] >= w[0] * (1.0 - TREND_WOBBLE));
    let quickening = intervals
        .windows(2)
        .all(|w| w[1] <= w[0] * (1.0 + TREND_WOBBLE));
    let steps = |up: bool| {
        intervals
            .windows(2)
            .filter(|w| {
                if up {
                    w[1] > w[0] * (1.0 + TREND_WOBBLE)
                } else {
                    w[1] < w[0] * (1.0 - TREND_WOBBLE)
                }
            })
            .count()
    };
    if slowing && last >= first * (1.0 + TREND_MIN) && steps(true) >= TREND_STEPS {
        Trend::Slowing
    } else if quickening && last <= first * (1.0 - TREND_MIN) && steps(false) >= TREND_STEPS {
        Trend::Quickening
    } else {
        Trend::Steady
    }
}

fn on_grid(t: f64, g: Grid) -> bool {
    let on = g.anchor + ((t - g.anchor) / g.period).round() * g.period;
    (t - on).abs() < 1.0e-9
}

/// Transition beats that are really the neighbours' grid beats (the tempo
/// has not moved yet, or already has) are put exactly on those grids.
fn snap_to_grids(mut beats: Vec<f64>, before: Grid, after: Grid) -> Vec<f64> {
    let snap = |t: f64, g: Grid| {
        let on = g.anchor + ((t - g.anchor) / g.period).round() * g.period;
        ((t - on).abs() <= GRID_SNAP * g.period).then_some(on)
    };
    for t in beats.iter_mut() {
        match snap(*t, before) {
            Some(on) => *t = on,
            None => break,
        }
    }
    for t in beats.iter_mut().rev() {
        match snap(*t, after) {
            Some(on) => *t = on,
            None => break,
        }
    }
    beats
}

/// Beats strictly between `from` and `to` — the last beat of one steady
/// section and the first of the next. When the gap is a whole number of
/// either section's periods and those beats land on onsets about as well as
/// a free track, the tempo simply changed there; otherwise the beats are
/// tracked with the period gliding from one section's to the other's, so a
/// ritardando or a push is followed beat by beat.
fn bridge(onsets: &Onsets, from: f64, to: f64, p_from: f64, p_to: f64) -> Vec<f64> {
    let gap = to - from;
    if gap <= 0.0 {
        return Vec::new();
    }
    let strength = |beats: &[f64]| -> f32 {
        let frames = beat_frames(onsets, beats);
        if frames.is_empty() {
            return f32::INFINITY;
        }
        frames
            .iter()
            .map(|&f| window_max(&onsets.flux, f, 2))
            .sum::<f32>()
            / frames.len() as f32
    };
    let free = track_between(onsets, from, to, p_from, p_to);
    let free_strength = strength(&free);
    let constant = [p_from, p_to].into_iter().filter_map(|p| {
        let n = (gap / p).round();
        (n >= 1.0 && (gap - n * p).abs() < GRID_TOLERANCE as f64 * p).then(|| {
            // Hold the old tempo up to `to`, or start the new one right after
            // `from`: either way, evenly spaced.
            let step = gap / n;
            (1..n as i64)
                .map(|k| from + k as f64 * step)
                .collect::<Vec<f64>>()
        })
    });
    constant
        .filter(|beats| strength(beats) >= BRIDGE_PREFER_CONSTANT * free_strength)
        .max_by(|a, b| strength(a).total_cmp(&strength(b)))
        .unwrap_or(free)
}

/// Beats between the pinned beats at `from` and `to`, tracked so that each
/// interval changes as little as it can from the one before: the tempo may
/// hold, glide or fall away in any shape, but not jump without an onset to
/// show for it. The interval before `from` is `p_from`; the one after `to`
/// is `p_to`.
fn track_between(onsets: &Onsets, from: f64, to: f64, p_from: f64, p_to: f64) -> Vec<f64> {
    let fps = onsets.fps as f64;
    let last = onsets.flux.len().saturating_sub(1);
    let fa = ((from * fps).round().max(0.0) as usize).min(last);
    let fb = ((to * fps).round().max(0.0) as usize).min(last);
    if fb <= fa + 1 {
        return Vec::new();
    }
    let n = fb - fa + 1;
    let (short, long) = (p_from.min(p_to) * fps, p_from.max(p_to) * fps);
    let d_min = ((BRIDGE_SHORTEST * short).floor() as usize).max(1);
    let d_max = ((BRIDGE_LONGEST * long).ceil() as usize)
        .min(n - 1)
        .max(d_min);
    let width = d_max - d_min + 1;
    let cost = |a: f64, b: f64| {
        let r = (b / a).ln() as f32;
        BRIDGE_TIGHTNESS * r * r
    };
    // score[t][d - d_min]: best path with a beat at t whose interval from
    // the beat before is d frames.
    let mut score = vec![f32::NEG_INFINITY; n * width];
    let mut back = vec![u16::MAX; n * width];
    for t in 1..n {
        let onset = if t + 1 == n {
            0.0
        } else {
            window_max(&onsets.flux, fa + t, 1)
        };
        for d in d_min..=d_max.min(t) {
            let tau = t - d;
            let (best, arg) = if tau == 0 {
                (-cost(p_from * fps, d as f64), u16::MAX)
            } else {
                let mut best = f32::NEG_INFINITY;
                let mut arg = u16::MAX;
                let lo = ((d as f64 / 1.6).floor() as usize).max(d_min);
                let hi = ((d as f64 * 1.6).ceil() as usize).min(d_max).min(tau);
                for e in lo..=hi {
                    let prev = score[tau * width + (e - d_min)];
                    if prev == f32::NEG_INFINITY {
                        continue;
                    }
                    let v = prev - cost(e as f64, d as f64);
                    if v > best {
                        best = v;
                        arg = e as u16;
                    }
                }
                (best, arg)
            };
            if best > f32::NEG_INFINITY {
                score[t * width + (d - d_min)] = best + onset;
                back[t * width + (d - d_min)] = arg;
            }
        }
    }
    // Close on `to`, whose next interval is `p_to`.
    let end = n - 1;
    let Some(d_end) = (d_min..=d_max.min(end))
        .filter(|&d| score[end * width + (d - d_min)] > f32::NEG_INFINITY)
        .max_by(|&a, &b| {
            let va = score[end * width + (a - d_min)] - cost(a as f64, p_to * fps);
            let vb = score[end * width + (b - d_min)] - cost(b as f64, p_to * fps);
            va.total_cmp(&vb)
        })
    else {
        return Vec::new();
    };
    let mut beats = Vec::new();
    let (mut t, mut d) = (end, d_end);
    loop {
        let e = back[t * width + (d - d_min)];
        t -= d;
        if t == 0 || e == u16::MAX {
            break;
        }
        beats.push(refine_peak(&onsets.flux, fa + t) / fps);
        d = e as usize;
    }
    beats.reverse();
    beats
}

fn consistent_with(t: f64, period: f64, anchor: f64) -> bool {
    let tol = GRID_TOLERANCE as f64 * period;
    let d = t - (anchor + ((t - anchor) / period).round() * period);
    d.abs() < tol || (d.abs() - 0.5 * period).abs() < tol
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

/// Tempo of each beat's interval to the next; the last beat repeats its
/// neighbour's.
fn per_beat_tempo(seconds: &[f64]) -> Vec<(f64, f32)> {
    (0..seconds.len())
        .map(|i| {
            let (a, b) = if i + 1 < seconds.len() {
                (i, i + 1)
            } else {
                (i.saturating_sub(1), i)
            };
            let ibi = (seconds[b] - seconds[a]).max(1e-3);
            (seconds[i], (60.0 / ibi) as f32)
        })
        .collect()
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
        let onsets = Onsets {
            fps: ONSET_FPS,
            flux: vec![0.0; 6_000],
            low: vec![0.0; 6_000],
        };
        let (locked, fits) = lock_steady_sections(&onsets, &tracked, &[section], 4);
        assert_eq!(fits.len(), 1);
        assert!(fits[0].grid.is_some());
        assert_eq!(fits[0].bpm, 120.0);
        assert_eq!(locked.len(), 108);
        for (i, t) in locked.iter().enumerate() {
            assert!((t - (1.0 + i as f64 * 0.5)).abs() < 1e-9, "beat {i}: {t}");
        }
    }

    #[test]
    fn a_ritardando_into_a_new_section_is_followed_beat_by_beat() {
        // 8 bars at 108, a transition bar slowing 108 → 100 → 97 → 85, 12
        // bars at 64, then the song at 120 for 40 bars.
        let run = |bpm: f64, start: f64, count: usize| -> Vec<f64> {
            (0..count).map(|i| start + i as f64 * 60.0 / bpm).collect()
        };
        let mut truth = run(108.0, 0.5, 32);
        let transition = truth.len();
        let mut t = truth[transition - 1] + 60.0 / 108.0;
        for bpm in [108.0, 100.0, 97.0, 85.0] {
            truth.push(t);
            t += 60.0 / bpm;
        }
        truth.extend(run(64.0, t, 48));
        let t = truth.last().unwrap() + 60.0 / 64.0;
        truth.extend(run(120.0, t, 160));
        let seconds = truth.last().unwrap() + 1.0;
        let audio = render(&truth, 4, seconds);
        let r = analyze(&audio, RhythmOptions::default());

        let bpms: Vec<f32> = r.sections.iter().map(|s| s.bpm).collect();
        assert_eq!(bpms, vec![108.0, 64.0, 120.0], "{:?}", r.sections);
        // Every beat of the transition is found where it was played, so its
        // per-beat tempo reads 108, 100, 97, 85.
        let found: Vec<f64> = r.beats.iter().map(|b| b.seconds).collect();
        let at = |want: f64| {
            found
                .iter()
                .copied()
                .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
                .unwrap()
        };
        let heard: Vec<f64> = truth[transition..=transition + 4]
            .iter()
            .map(|&w| at(w))
            .collect();
        for (h, w) in heard.iter().zip(&truth[transition..]) {
            assert!(
                (h - w).abs() < 0.02,
                "transition beat {w:.3} found at {h:.3}"
            );
        }
        let tempos: Vec<f64> = heard.windows(2).map(|w| 60.0 / (w[1] - w[0])).collect();
        for (got, want) in tempos.iter().zip([108.0, 100.0, 97.0, 85.0]) {
            assert!((got - want).abs() < 2.0, "{tempos:?}");
        }
        // Only the beats that fit neither section's grid are marked off it,
        // so the tempo map gives them a tempo each.
        let locked = |want: f64| {
            r.beats
                .iter()
                .min_by(|a, b| {
                    (a.seconds - want)
                        .abs()
                        .total_cmp(&(b.seconds - want).abs())
                })
                .unwrap()
                .locked
        };
        let flags: Vec<bool> = truth[transition - 2..=transition + 5]
            .iter()
            .map(|&w| locked(w))
            .collect();
        assert_eq!(
            flags,
            vec![true, true, true, true, false, false, true, true],
            "{flags:?}"
        );
        // And every bar line stays on its downbeat through the change.
        let downbeats: Vec<f64> = truth.iter().step_by(4).copied().collect();
        assert!(recall(&r.downbeats(), &downbeats) > 0.95);
    }

    #[test]
    fn a_ritardando_into_a_half_time_section_reads_it_at_the_slow_tempo() {
        // 8 bars at 108, a bar slowing 108 → 100 → 76, then a 65 section
        // whose drums play 8th notes (kick on the bar, snare on every other
        // 8th: on its own it reads as 130), then 120.
        let run = |bpm: f64, start: f64, count: usize| -> Vec<f64> {
            (0..count).map(|i| start + i as f64 * 60.0 / bpm).collect()
        };
        let mut intro = run(108.0, 0.5, 32);
        let mut t = intro[31] + 60.0 / 108.0;
        for bpm in [108.0, 100.0, 76.0] {
            intro.push(t);
            t += 60.0 / bpm;
        }
        intro.push(t); // beat 4 of the transition bar, then the slow bar line
        t += 60.0 / 76.0;
        let slow_start = t;
        let eighths = run(130.0, slow_start, 16 * 8);
        let song_start = slow_start + 16.0 * 4.0 * 60.0 / 65.0;
        let song = run(120.0, song_start, 160);
        let seconds = song.last().unwrap() + 1.0;
        let mut audio = render(&intro, 4, seconds);
        for (i, v) in render(&eighths, 8, seconds).into_iter().enumerate() {
            audio[i] += v;
        }
        for (i, v) in render(&song, 4, seconds).into_iter().enumerate() {
            audio[i] += v;
        }
        let r = analyze(&audio, RhythmOptions::default());
        let bpms: Vec<f32> = r.sections.iter().map(|s| s.bpm).collect();
        assert_eq!(bpms, vec![108.0, 65.0, 120.0], "{:?}", r.sections);
        // The slow section starts on its bar line, right after the slowdown.
        assert!(
            (r.sections[1].start_seconds - slow_start).abs() < 0.03,
            "{:?} vs {slow_start}",
            r.sections[1]
        );
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

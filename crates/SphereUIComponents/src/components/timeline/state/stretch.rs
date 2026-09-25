//! Audio-clip time-stretch / pitch state and the pure math that drives it.
//!
//! This is **clip-level, non-destructive** playback-transform metadata. It never
//! mutates the source audio; the playback/export processors (added in a later
//! slice) read this state to transform the source on the fly. The data model and
//! math live here, decoupled from the audio engine, so they can be unit-tested in
//! isolation and serialized without pulling in realtime code.
//!
//! Source of truth: `AudioClipStretchState` lives on [`super::ClipState`]. It is
//! present on every clip but only meaningful for audio clips — MIDI clips carry a
//! default (`StretchMode::Off`) instance that is ignored.

use sphere_audio_editor::ClipEnvelope;

/// How an audio clip's playback timing is transformed.
///
/// See the per-variant docs and `tasks` spec §2 for behaviour. Tags are stable
/// for serialization via [`StretchMode::to_tag`] / [`StretchMode::from_tag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StretchMode {
    /// No time stretch. Playback rate is normal; clip duration follows the
    /// source duration at the current project sample rate.
    #[default]
    Off,
    /// Classic sampler/tape behaviour: changing clip length changes pitch.
    /// No pitch preservation.
    Resample,
    /// Clip follows project tempo (loops). `stretch_ratio = source_bpm /
    /// project_bpm`. Constant-tempo today; API is shaped for tempo maps later.
    TempoSync,
    /// User sets duration / ratio / percent directly; the three stay in sync.
    Manual,
    /// Warp-marker mode. Marker data is stored on the clip and exposed to the
    /// engine; playback currently uses the global stretch ratio until the
    /// per-segment warp processor is enabled.
    Warp,
}

impl StretchMode {
    pub fn to_tag(self) -> u8 {
        match self {
            StretchMode::Off => 0,
            StretchMode::Resample => 1,
            StretchMode::TempoSync => 2,
            StretchMode::Manual => 3,
            StretchMode::Warp => 4,
        }
    }

    pub fn from_tag(tag: u8) -> Self {
        match tag {
            1 => StretchMode::Resample,
            2 => StretchMode::TempoSync,
            3 => StretchMode::Manual,
            4 => StretchMode::Warp,
            // Unknown / 0 → Off (also the backward-compat default).
            _ => StretchMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StretchMode::Off => "Off",
            StretchMode::Resample => "Resample",
            StretchMode::TempoSync => "Tempo Sync",
            StretchMode::Manual => "Manual",
            StretchMode::Warp => "Warp",
        }
    }
}

/// What the user chose for a clip's timing, as the Inspector presents it.
///
/// [`StretchMode`] is the persisted, engine-facing tag and keeps its five
/// variants for project compatibility. People think in four answers to "what
/// decides how long this clip plays?": nothing (`Off`), a speed they set
/// (`Speed` — stored as `Manual` or `Resample` depending on the pitch choice),
/// the project tempo (`Tempo`), or warp markers (`Warp`). Whether pitch follows
/// the speed is a separate, orthogonal choice — see
/// [`AudioClipStretchState::keeps_pitch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StretchTiming {
    Off,
    Speed,
    Tempo,
    Warp,
}

impl StretchTiming {
    pub const ALL: [StretchTiming; 4] = [
        StretchTiming::Off,
        StretchTiming::Speed,
        StretchTiming::Tempo,
        StretchTiming::Warp,
    ];

    pub fn label(self) -> &'static str {
        match self {
            StretchTiming::Off => "Off",
            StretchTiming::Speed => "Speed",
            StretchTiming::Tempo => "Tempo",
            StretchTiming::Warp => "Warp",
        }
    }
}

/// Stretch algorithm selection.
///
/// Only some variants are backed by real DSP today; the rest are honest
/// placeholders that alias onto an implemented algorithm until their dedicated
/// DSP lands (see the per-variant notes). The processor slice maps these tags to
/// concrete processors; the UI must not claim an aliased mode is its own engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StretchAlgorithm {
    /// Pick an algorithm from content type. Resolved by the processor slice.
    #[default]
    Auto,
    /// Reserved high-quality mode. **Aliases to PhaseVocoder** until a dedicated
    /// élastique-class engine is integrated.
    ElastiqueLike,
    /// General-purpose musical material. **Real** (basic phase vocoder) once the
    /// processor slice lands.
    PhaseVocoder,
    /// Drums / percussive material. **Aliases to PhaseVocoder** until transient-
    /// aware stretching is implemented.
    Transient,
    /// Vocals / monophonic instruments. **Aliases to PhaseVocoder** for now.
    Solo,
    /// Pads / ambience. **Aliases to PhaseVocoder** for now.
    Texture,
    /// Simple sample-rate / playback-rate conversion. **Real** resampling.
    ResampleOnly,
}

impl StretchAlgorithm {
    pub fn to_tag(self) -> u8 {
        match self {
            StretchAlgorithm::Auto => 0,
            StretchAlgorithm::ElastiqueLike => 1,
            StretchAlgorithm::PhaseVocoder => 2,
            StretchAlgorithm::Transient => 3,
            StretchAlgorithm::Solo => 4,
            StretchAlgorithm::Texture => 5,
            StretchAlgorithm::ResampleOnly => 6,
        }
    }

    pub fn from_tag(tag: u8) -> Self {
        match tag {
            1 => StretchAlgorithm::ElastiqueLike,
            2 => StretchAlgorithm::PhaseVocoder,
            3 => StretchAlgorithm::Transient,
            4 => StretchAlgorithm::Solo,
            5 => StretchAlgorithm::Texture,
            6 => StretchAlgorithm::ResampleOnly,
            _ => StretchAlgorithm::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StretchAlgorithm::Auto => "Auto",
            StretchAlgorithm::ElastiqueLike => "Élastique",
            StretchAlgorithm::PhaseVocoder => "Phase Vocoder",
            StretchAlgorithm::Transient => "Transient",
            StretchAlgorithm::Solo => "Solo",
            StretchAlgorithm::Texture => "Texture",
            StretchAlgorithm::ResampleOnly => "Resample Only",
        }
    }
}

/// A warp marker pinning a source sample position to a timeline beat.
///
/// Marker positions are validated before they reach the engine. `timeline_beat`
/// is an absolute project beat, while `source_sample` is in the source file's
/// native sample-rate domain. The stored map is ready for the per-segment DSP
/// path and does not alter playback while that path is unavailable.
#[derive(Debug, Clone, PartialEq)]
pub struct WarpMarker {
    pub id: u64,
    pub source_sample: u64,
    pub timeline_beat: f64,
    pub locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempoRelation {
    Raw,
    Half,
    Double,
    TwoThirds,
    ThreeHalves,
    FourThirds,
    ThreeQuarters,
    ProjectPrior,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TempoCandidate {
    pub bpm: f32,
    pub confidence: f32,
    pub relation: TempoRelation,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TempoDetectionResult {
    pub bpm: f32,
    pub confidence: f32,
    pub low_confidence: bool,
    pub alternatives: Vec<f32>,
    pub candidates: Vec<TempoCandidate>,
    pub selection_reason: String,
}

/// Detection confidence below which Auto Find must NOT auto-commit
/// `clip.stretch.source_bpm`; instead it surfaces candidates and waits for the
/// user to pick or confirm (spec Fix 1/8).
const TEMPO_LOW_CONFIDENCE: f32 = 0.35;
/// Additive weight of the project-tempo soft prior (spec Fix 5).
const TEMPO_PROJECT_PRIOR_WEIGHT: f32 = 0.15;
/// A candidate within this fraction of the project tempo counts as "near
/// project" for the promotion rule (spec Fix 5: ±3%).
const TEMPO_PROJECT_NEAR_TOLERANCE: f32 = 0.03;
/// A near-project candidate is promoted above the raw winner only when its raw
/// score is at least this fraction of the best raw score (spec Fix 5: ±20%).
const TEMPO_PROJECT_NEAR_SCORE_MARGIN: f32 = 0.80;
/// Decisive bonus that lets a strong near-project candidate outrank a slightly
/// stronger raw candidate without forcing the project tempo (spec Fix 5).
const TEMPO_PROJECT_NEAR_PROMOTION: f32 = 0.25;
/// Coarse BPM step swept across the whole search range (spec Fix 4).
const TEMPO_COARSE_STEP_BPM: f32 = 0.25;
/// Fine BPM step swept across the project-tempo neighbourhood (spec Fix 4).
const TEMPO_FINE_STEP_BPM: f32 = 0.1;
/// Half-width of the project-tempo neighbourhood scanned at the fine step
/// (spec Fix 4: project_bpm ± 15%).
const TEMPO_PROJECT_PRIOR_SPAN: f32 = 0.15;
const TEMPO_ANALYSIS_RATE: f32 = 11_025.0;
const TEMPO_FRAME: usize = 512;
const TEMPO_HOP: usize = 256;
/// Preferred analysis-window length around the strongest region (spec Fix 2:
/// 16–32 s); the window is capped at [`TEMPO_MAX_ANALYSIS_SECONDS`].
const TEMPO_PREFERRED_ANALYSIS_SECONDS: f32 = 24.0;
const TEMPO_MAX_ANALYSIS_SECONDS: f32 = 60.0;

fn remove_dc(samples: &[f32]) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    let mean = samples
        .iter()
        .copied()
        .filter(|s| s.is_finite())
        .sum::<f32>()
        / samples.len() as f32;
    samples
        .iter()
        .map(|s| if s.is_finite() { *s - mean } else { 0.0 })
        .collect()
}

fn normalize_peak_safe(samples: &[f32]) -> Vec<f32> {
    let peak = samples
        .iter()
        .copied()
        .filter(|s| s.is_finite())
        .map(f32::abs)
        .fold(0.0_f32, f32::max);
    if peak <= f32::EPSILON {
        return samples.to_vec();
    }
    samples
        .iter()
        .map(|s| (s / peak).clamp(-1.0, 1.0))
        .collect()
}

fn downsample_mono(samples: &[f32], source_rate: f32, target_rate: f32) -> Vec<f32> {
    if samples.is_empty() || source_rate <= 0.0 || target_rate <= 0.0 {
        return Vec::new();
    }
    let step = (source_rate / target_rate).round().max(1.0) as usize;
    samples.iter().step_by(step).copied().collect()
}

fn rms_frame(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum = frame.iter().map(|s| s * s).sum::<f32>();
    (sum / frame.len() as f32).sqrt()
}

/// Spectral-flux-style onset envelope (spec Fix 3): per-frame RMS energy →
/// positive energy flux → adaptive local-mean subtraction (half-wave rectified)
/// → light smoothing → peak normalisation. Frames whose energy is far below the
/// loudest frame are ignored so silence/near-silence does not generate spurious
/// onsets. `local_mean_window` is the moving-average length (in frames, ~1 s)
/// used as the adaptive threshold.
fn build_onset_envelope(
    samples: &[f32],
    frame: usize,
    hop: usize,
    local_mean_window: usize,
) -> Vec<f32> {
    if samples.len() < frame {
        return Vec::new();
    }
    let energy: Vec<f32> = samples.windows(frame).step_by(hop).map(rms_frame).collect();
    let n = energy.len();
    if n < 4 {
        return Vec::new();
    }

    let energy_peak = energy.iter().copied().fold(0.0_f32, f32::max);
    let energy_floor = energy_peak * 0.02;

    // Positive energy flux (onset emphasis).
    let mut flux = vec![0.0_f32; n];
    for i in 1..n {
        flux[i] = if energy[i] < energy_floor {
            0.0
        } else {
            (energy[i] - energy[i - 1]).max(0.0)
        };
    }

    // Subtract a local moving average (adaptive threshold) and half-wave rectify.
    let window = local_mean_window.max(3);
    let mut onset = vec![0.0_f32; n];
    for i in 0..n {
        let start = i.saturating_sub(window / 2);
        let end = (i + window / 2 + 1).min(n);
        let local_mean = flux[start..end].iter().sum::<f32>() / (end - start).max(1) as f32;
        onset[i] = (flux[i] - local_mean).max(0.0);
    }

    // Light smoothing (3 frames) to stabilise the peaks before autocorrelation.
    let smooth = 3usize;
    let smoothed: Vec<f32> = (0..n)
        .map(|i| {
            let start = i.saturating_sub(smooth / 2);
            let end = (i + smooth / 2 + 1).min(n);
            onset[start..end].iter().sum::<f32>() / (end - start).max(1) as f32
        })
        .collect();

    let peak = smoothed.iter().copied().fold(0.0_f32, f32::max);
    if peak <= f32::EPSILON {
        return smoothed;
    }
    smoothed.into_iter().map(|v| v / peak).collect()
}

fn autocorrelation_score(envelope: &[f32], lag: usize) -> f32 {
    if lag == 0 || lag >= envelope.len() {
        return 0.0;
    }
    let mut score = 0.0_f32;
    for i in lag..envelope.len() {
        score += envelope[i] * envelope[i - lag];
    }
    score / (envelope.len() - lag).max(1) as f32
}

fn lag_from_bpm(bpm: f32, env_rate: f32) -> f32 {
    60.0 * env_rate / bpm.max(f32::EPSILON)
}

fn interpolated_autocorr_score(envelope: &[f32], lag: f32) -> f32 {
    let center = lag.floor() as usize;
    if center == 0 || center >= envelope.len() {
        return 0.0;
    }
    let frac = lag - center as f32;
    let s1 = autocorrelation_score(envelope, center);
    if center + 1 < envelope.len() {
        let s2 = autocorrelation_score(envelope, center + 1);
        s1 * (1.0 - frac) + s2 * frac
    } else {
        s1
    }
}

/// Multi-beat comb autocorrelation (spec Fix 4): rewards a lag that is also
/// reinforced two and three beats later. This suppresses weak single-lag matches
/// and spurious half/double-tempo peaks, helping the true musical period win.
fn comb_autocorr_score(envelope: &[f32], lag: f32) -> f32 {
    let s1 = interpolated_autocorr_score(envelope, lag);
    let s2 = interpolated_autocorr_score(envelope, lag * 2.0);
    let s3 = interpolated_autocorr_score(envelope, lag * 3.0);
    s1 + 0.5 * s2 + 0.25 * s3
}

fn tempo_family_ratios() -> [(f32, TempoRelation); 9] {
    [
        (0.5, TempoRelation::Half),
        (2.0, TempoRelation::Double),
        (2.0 / 3.0, TempoRelation::TwoThirds),
        (1.5, TempoRelation::ThreeHalves),
        (4.0 / 3.0, TempoRelation::FourThirds),
        (0.75, TempoRelation::ThreeQuarters),
        (5.0 / 6.0, TempoRelation::FourThirds),
        (6.0 / 5.0, TempoRelation::ThreeHalves),
        (1.0, TempoRelation::Raw),
    ]
}

fn expand_tempo_family(
    base_bpm: f32,
    base_score: f32,
    min_bpm: f32,
    max_bpm: f32,
) -> Vec<TempoCandidate> {
    let mut out = Vec::new();
    for (ratio, relation) in tempo_family_ratios() {
        let bpm = base_bpm * ratio;
        if bpm >= min_bpm && bpm <= max_bpm {
            let penalty = if ratio < 0.66 || ratio > 1.51 {
                0.92
            } else {
                1.0
            };
            out.push(TempoCandidate {
                bpm,
                confidence: base_score * penalty,
                relation,
            });
        }
    }
    out
}

fn merge_tempo_candidates(
    candidates: Vec<TempoCandidate>,
    min_bpm: f32,
    max_bpm: f32,
) -> Vec<TempoCandidate> {
    let mut merged: Vec<TempoCandidate> = Vec::new();
    for candidate in candidates {
        if candidate.bpm < min_bpm || candidate.bpm > max_bpm || !candidate.confidence.is_finite() {
            continue;
        }
        if let Some(existing) = merged
            .iter_mut()
            .find(|c| (c.bpm - candidate.bpm).abs() < 0.6)
        {
            if candidate.confidence > existing.confidence {
                *existing = candidate;
            }
        } else {
            merged.push(candidate);
        }
    }
    merged.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged
}

/// Soft, additive project-tempo prior (spec Fix 5):
/// `exp(-|log2(bpm / project)| * 8) * weight`. Peaks at the project tempo and
/// decays quickly with octave distance, so it nudges ranking without forcing the
/// project tempo onto unrelated material.
fn project_prior_bonus(bpm: f32, project_bpm: f32) -> f32 {
    if project_bpm <= 0.0 || bpm <= 0.0 {
        return 0.0;
    }
    let octave_dist = (bpm / project_bpm).log2().abs();
    (-octave_dist * 8.0).exp() * TEMPO_PROJECT_PRIOR_WEIGHT
}

/// Dense BPM→score scan of the onset envelope. Sweeping by BPM (not by integer
/// autocorrelation lag) resolves tempi that fall *between* coarse lags — e.g.
/// 118 vs 124 near a 120 project tempo — and a finer pass over the project
/// neighbourhood makes the true tempo reliably resolvable (spec Fix 4).
struct TempoScan {
    bpms: Vec<f32>,
    /// Scores normalised so the strongest bin == 1.0.
    scores: Vec<f32>,
    /// `(peak - mean) / peak` over the raw scan, 0..1 — how much the best tempo
    /// stands out from the field. Drives the reported detection confidence.
    salience: f32,
}

impl TempoScan {
    fn score_at(&self, bpm: f32) -> f32 {
        let mut best = 0usize;
        let mut best_dist = f32::MAX;
        for (i, b) in self.bpms.iter().enumerate() {
            let d = (b - bpm).abs();
            if d < best_dist {
                best_dist = d;
                best = i;
            }
        }
        self.scores.get(best).copied().unwrap_or(0.0)
    }

    fn best_in_range(&self, lo: f32, hi: f32) -> Option<(f32, f32)> {
        let mut out: Option<(f32, f32)> = None;
        for (b, s) in self.bpms.iter().zip(self.scores.iter()) {
            if *b >= lo && *b <= hi && out.is_none_or(|(_, best)| *s > best) {
                out = Some((*b, *s));
            }
        }
        out
    }
}

fn scan_tempo_scores(
    envelope: &[f32],
    env_rate: f32,
    min_bpm: f32,
    max_bpm: f32,
    project_bpm: Option<f32>,
) -> TempoScan {
    let slow_debias =
        |bpm: f32| 0.85 + 0.15 * ((bpm - min_bpm) / (max_bpm - min_bpm).max(1.0)).clamp(0.0, 1.0);
    let score_bpm = |bpm: f32| {
        comb_autocorr_score(envelope, lag_from_bpm(bpm, env_rate)).max(0.0) * slow_debias(bpm)
    };

    let mut points: Vec<(f32, f32)> = Vec::new();
    let mut bpm = min_bpm;
    while bpm <= max_bpm + 1e-3 {
        points.push((bpm, score_bpm(bpm)));
        bpm += TEMPO_COARSE_STEP_BPM;
    }
    // Fine pass over the project neighbourhood (spec Fix 4: project ± 15%).
    if let Some(p) = project_bpm.filter(|p| p.is_finite() && *p > 0.0) {
        let lo = (p * (1.0 - TEMPO_PROJECT_PRIOR_SPAN)).max(min_bpm);
        let hi = (p * (1.0 + TEMPO_PROJECT_PRIOR_SPAN)).min(max_bpm);
        let mut b = lo;
        while b <= hi + 1e-3 {
            points.push((b, score_bpm(b)));
            b += TEMPO_FINE_STEP_BPM;
        }
    }
    points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let peak = points.iter().map(|(_, s)| *s).fold(0.0_f32, f32::max);
    let mean = if points.is_empty() {
        0.0
    } else {
        points.iter().map(|(_, s)| *s).sum::<f32>() / points.len() as f32
    };
    let salience = if peak > f32::EPSILON {
        ((peak - mean) / peak).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let norm = peak.max(f32::EPSILON);
    TempoScan {
        bpms: points.iter().map(|(b, _)| *b).collect(),
        scores: points.iter().map(|(_, s)| s / norm).collect(),
        salience,
    }
}

/// Non-maximum-suppressed local peaks of the dense scan, strongest first.
fn pick_tempo_peaks(scan: &TempoScan) -> Vec<TempoCandidate> {
    let mut order: Vec<usize> = (0..scan.scores.len()).collect();
    order.sort_by(|a, b| {
        scan.scores[*b]
            .partial_cmp(&scan.scores[*a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut peaks: Vec<TempoCandidate> = Vec::new();
    for i in order {
        let bpm = scan.bpms[i];
        let score = scan.scores[i];
        if score < 0.15 {
            break;
        }
        // Suppress neighbours within 2 BPM so peaks are distinct tempi.
        if peaks.iter().any(|p| (p.bpm - bpm).abs() < 2.0) {
            continue;
        }
        peaks.push(TempoCandidate {
            bpm,
            confidence: score,
            relation: TempoRelation::Raw,
        });
        if peaks.len() >= 8 {
            break;
        }
    }
    peaks
}

/// Build the picker's alternative chips, always surfacing project-relative
/// options (project tempo, half/double, and the best candidate within ±2/5/10%)
/// when a project tempo exists, so a near-project tempo such as 118 against a
/// 120 project is never hidden (spec Fix 6).
fn build_tempo_alternatives(
    selected_bpm: f32,
    candidates: &[TempoCandidate],
    scan: &TempoScan,
    project_bpm: Option<f32>,
    min_bpm: f32,
    max_bpm: f32,
) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::new();
    let in_range = |bpm: f32| bpm.is_finite() && bpm >= min_bpm && bpm <= max_bpm;

    if in_range(selected_bpm) {
        out.push(selected_bpm);
    }
    for c in candidates.iter().take(6) {
        if in_range(c.bpm) {
            out.push(c.bpm);
        }
    }
    if let Some(p) = project_bpm.filter(|p| p.is_finite() && *p > 0.0) {
        for bpm in [p, p * 0.5, p * 2.0] {
            if in_range(bpm) {
                out.push(bpm);
            }
        }
        for tol in [0.02_f32, 0.05, 0.10] {
            if let Some((bpm, _)) = scan.best_in_range(p * (1.0 - tol), p * (1.0 + tol)) {
                if in_range(bpm) {
                    out.push(bpm);
                }
            }
        }
    }

    out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    out.dedup_by(|a, b| (*a - *b).abs() < 0.6);
    out.truncate(8);
    out
}

/// Pick the strongest, longest, non-silent analysis window for offline tempo
/// detection (spec Fix 2). Skips leading near-silence (relative −22 dB / absolute
/// −45 dBFS floor), then slides a preferred-length window to find the
/// highest-energy region so a weak intro is never weighted like the hook. Caps at
/// ~60 s and prefers ~24 s; short clips fall back to their full non-silent span.
pub fn prepare_mono_for_tempo_analysis(samples: &[f32], sample_rate: f32) -> (Vec<f32>, f32) {
    if samples.is_empty() || sample_rate <= 0.0 {
        return (Vec::new(), 0.0);
    }
    let max_samples = (sample_rate * TEMPO_MAX_ANALYSIS_SECONDS).round() as usize;
    let preferred =
        ((sample_rate * TEMPO_PREFERRED_ANALYSIS_SECONDS).round() as usize).clamp(1, max_samples);

    // Coarse RMS envelope over ~93 ms blocks to locate strong vs silent regions.
    let block = ((sample_rate * 0.093).round() as usize).max(1);
    let blocks: Vec<f32> = samples.chunks(block).map(rms_frame).collect();
    let peak_rms = blocks.iter().copied().fold(0.0_f32, f32::max);
    if peak_rms <= f32::EPSILON {
        let end = samples.len().min(max_samples);
        return (samples[..end].to_vec(), end as f32 / sample_rate);
    }

    // Skip leading near-silence: ~8% of peak RMS, with an absolute −45 dBFS floor.
    let abs_floor = 10f32.powf(-45.0 / 20.0);
    let active_threshold = (peak_rms * 0.08).max(abs_floor);
    let first_active = blocks
        .iter()
        .position(|&r| r >= active_threshold)
        .unwrap_or(0);

    // Slide a preferred-length window over the active region; keep the loudest.
    let pref_blocks = (preferred / block).max(1);
    let mut prefix = Vec::with_capacity(blocks.len() + 1);
    prefix.push(0.0_f32);
    for &r in &blocks {
        prefix.push(prefix.last().copied().unwrap_or(0.0) + r * r);
    }
    let mut best_start_block = first_active;
    let mut best_energy = f32::MIN;
    let mut start_block = first_active;
    while start_block < blocks.len() {
        let end_block = (start_block + pref_blocks).min(blocks.len());
        let energy = prefix[end_block] - prefix[start_block];
        if energy > best_energy {
            best_energy = energy;
            best_start_block = start_block;
        }
        if end_block >= blocks.len() {
            break;
        }
        start_block += (pref_blocks / 2).max(1);
    }

    let start = (best_start_block * block).min(samples.len());
    let end = samples.len().min(start + max_samples);
    let slice = if end > start {
        &samples[start..end]
    } else {
        samples
    };
    let duration = slice.len() as f32 / sample_rate;
    (slice.to_vec(), duration)
}

/// Choose the most musically useful BPM among scored candidates, using project
/// tempo as a soft prior when confidence scores are close.
pub fn choose_musical_bpm_candidate(
    raw_candidates: &[TempoCandidate],
    project_bpm: Option<f32>,
    min_bpm: f32,
    max_bpm: f32,
) -> Option<TempoDetectionResult> {
    let candidates = merge_tempo_candidates(raw_candidates.to_vec(), min_bpm, max_bpm);
    if candidates.is_empty() {
        return None;
    }
    let best_raw = candidates
        .iter()
        .map(|c| c.confidence)
        .fold(0.0_f32, f32::max);
    let project = project_bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0);

    // Rank by `raw_score + project_prior_bonus`, with a decisive promotion for a
    // strong candidate within ±3% of the project tempo (spec Fix 5). The prior
    // only nudges; it never forces the project tempo onto a weak near candidate.
    let mut selected_idx = 0usize;
    let mut selected_final = f32::MIN;
    let mut reason = "highest confidence".to_string();
    for (i, candidate) in candidates.iter().enumerate() {
        let mut final_score = candidate.confidence;
        let mut promoted = false;
        if let Some(p) = project {
            final_score += project_prior_bonus(candidate.bpm, p);
            let dist = (candidate.bpm - p).abs() / p;
            if dist <= TEMPO_PROJECT_NEAR_TOLERANCE
                && candidate.confidence >= best_raw * TEMPO_PROJECT_NEAR_SCORE_MARGIN
            {
                final_score += TEMPO_PROJECT_NEAR_PROMOTION;
                promoted = true;
            }
        }
        if final_score > selected_final {
            selected_final = final_score;
            selected_idx = i;
            reason = if promoted {
                format!(
                    "near project tempo ({:.1} BPM) with a competitive score",
                    project.unwrap_or(0.0)
                )
            } else if project.is_some() && candidate.confidence < best_raw - f32::EPSILON {
                "project-prior weighted".to_string()
            } else {
                "highest confidence".to_string()
            };
        }
    }
    let selected = candidates[selected_idx].clone();

    let mut alternatives: Vec<f32> = candidates.iter().take(10).map(|c| c.bpm).collect();
    alternatives.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    alternatives.dedup_by(|a, b| (*a - *b).abs() < 0.35);

    let confidence = selected.confidence.clamp(0.0, 1.0);
    Some(TempoDetectionResult {
        bpm: selected.bpm,
        confidence,
        low_confidence: confidence < TEMPO_LOW_CONFIDENCE,
        alternatives,
        candidates,
        selection_reason: reason,
    })
}

/// Lightweight offline tempo detector for UI analysis jobs. Callers should run
/// this on a background worker after decoding/mixing a clip to mono; it is not
/// a realtime DSP function.
pub fn detect_tempo_from_mono(
    samples: &[f32],
    sample_rate: f32,
    min_bpm: f32,
    max_bpm: f32,
    project_bpm: Option<f32>,
) -> Option<TempoDetectionResult> {
    if samples.len() < 4 || sample_rate <= 0.0 || min_bpm <= 0.0 || max_bpm <= min_bpm {
        return None;
    }

    let (windowed, _duration) = prepare_mono_for_tempo_analysis(samples, sample_rate);
    if windowed.len() < TEMPO_FRAME * 4 {
        return None;
    }

    let mono = normalize_peak_safe(&remove_dc(&windowed));
    let analysis_rate = TEMPO_ANALYSIS_RATE.min(sample_rate).max(1.0);
    let downsampled = downsample_mono(&mono, sample_rate, analysis_rate);
    if downsampled.len() < TEMPO_FRAME * 4 {
        return None;
    }

    let env_rate = analysis_rate / TEMPO_HOP as f32;
    // ~1 s adaptive-threshold window for the onset envelope.
    let local_mean_window = (env_rate).round().max(3.0) as usize;
    let envelope = build_onset_envelope(&downsampled, TEMPO_FRAME, TEMPO_HOP, local_mean_window);
    if envelope.len() < 8 {
        return None;
    }
    if envelope.iter().map(|v| v * v).sum::<f32>() <= f32::EPSILON {
        return None;
    }

    // Dense BPM scan (+ fine project neighbourhood) → local-maxima candidates.
    let scan = scan_tempo_scores(&envelope, env_rate, min_bpm, max_bpm, project_bpm);
    if scan.bpms.is_empty() {
        return None;
    }
    let raw_peaks = pick_tempo_peaks(&scan);
    if raw_peaks.is_empty() {
        return None;
    }

    let mut all_candidates: Vec<TempoCandidate> = Vec::new();
    all_candidates.extend(raw_peaks.iter().take(8).cloned());
    for peak in raw_peaks.iter().take(6) {
        all_candidates.extend(expand_tempo_family(
            peak.bpm,
            peak.confidence,
            min_bpm,
            max_bpm,
        ));
    }
    // Always seed project tempo and its half/double as candidates so they can be
    // selected/promoted and always appear among alternatives (spec Fix 5/6).
    if let Some(project) =
        project_bpm.filter(|bpm| bpm.is_finite() && *bpm >= min_bpm && *bpm <= max_bpm)
    {
        for bpm in [project, project * 0.5, project * 2.0] {
            if bpm >= min_bpm && bpm <= max_bpm {
                all_candidates.push(TempoCandidate {
                    bpm,
                    confidence: scan.score_at(bpm),
                    relation: TempoRelation::ProjectPrior,
                });
            }
        }
    }

    let mut result = choose_musical_bpm_candidate(&all_candidates, project_bpm, min_bpm, max_bpm)?;
    result.alternatives = build_tempo_alternatives(
        result.bpm,
        &result.candidates,
        &scan,
        project_bpm,
        min_bpm,
        max_bpm,
    );
    // Report confidence as candidate strength × how much the best tempo stands
    // out (salience). A flat, ambiguous scan → low confidence → the UI requires
    // the user to pick instead of auto-committing (spec Fix 1/8).
    let reported = (result.confidence * scan.salience).clamp(0.0, 1.0);
    result.confidence = reported;
    result.low_confidence = reported < TEMPO_LOW_CONFIDENCE;
    Some(result)
}

/// Unique BPM alternatives suitable for a compact picker.
pub fn tempo_picker_alternatives(result: &TempoDetectionResult) -> Vec<f32> {
    result.alternatives.clone()
}

/// Map an output-local sample position inside a stretched clip back to the
/// immutable source-window sample. This is the UI-side equivalent of the engine
/// clip source-position mapping.
pub fn clip_output_local_to_source_sample(
    output_local_sample: f64,
    source_start: u64,
    source_end: u64,
    effective_time_ratio: f64,
    reverse: bool,
) -> f64 {
    let ratio = if effective_time_ratio.is_finite() {
        effective_time_ratio.max(1e-6)
    } else {
        1.0
    };
    let advance = output_local_sample.max(0.0) / ratio;
    if reverse {
        (source_end as f64 - advance).max(source_start as f64)
    } else {
        (source_start as f64 + advance).min(source_end as f64)
    }
}

pub fn warp_timeline_beat_to_source_sample(
    timeline_beat: f64,
    source_start: u64,
    source_end: u64,
    global_ratio: f64,
    markers: &[WarpMarker],
) -> f64 {
    let source_start_f = source_start as f64;
    let source_end_f = source_end.max(source_start) as f64;
    if markers.is_empty() {
        return clip_output_local_to_source_sample(
            timeline_beat.max(0.0),
            source_start,
            source_end,
            global_ratio,
            false,
        );
    }

    if timeline_beat <= markers[0].timeline_beat {
        return markers[0]
            .source_sample
            .clamp(source_start, source_end.max(source_start)) as f64;
    }
    for pair in markers.windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        if timeline_beat >= a.timeline_beat && timeline_beat <= b.timeline_beat {
            let span = (b.timeline_beat - a.timeline_beat).max(f64::EPSILON);
            let t = ((timeline_beat - a.timeline_beat) / span).clamp(0.0, 1.0);
            let source =
                a.source_sample as f64 + (b.source_sample as f64 - a.source_sample as f64) * t;
            return source.clamp(source_start_f, source_end_f);
        }
    }
    markers
        .last()
        .map(|marker| {
            marker
                .source_sample
                .clamp(source_start, source_end.max(source_start)) as f64
        })
        .unwrap_or(source_start_f)
}

/// Non-destructive, clip-level stretch + pitch + clip-processing state.
///
/// Fields mirror the `tasks` spec §1. Note that `clip_timeline_start_beats` /
/// `clip_timeline_duration_beats` are an informational cache of the owning
/// clip's timeline placement — the authoritative position stays on
/// [`super::ClipState`] (`start_beat` / `duration_beats`). Likewise `gain_db`,
/// `pan`, and the fade fields are clip-processing values whose reconciliation
/// with the existing linear `ClipState::gain` is owned by the inspector/playback
/// slices; here they are inert stored data.
///
/// `dirty` is a transient "needs re-process / re-render" flag and is **not**
/// persisted (it always loads as `false`).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioClipStretchState {
    pub mode: StretchMode,
    pub algorithm: StretchAlgorithm,

    pub original_sample_rate: u32,
    pub project_sample_rate: u32,

    pub original_duration_samples: u64,
    pub source_start_samples: u64,
    pub source_end_samples: u64,

    pub clip_timeline_start_beats: f64,
    pub clip_timeline_duration_beats: f64,

    pub stretch_ratio: f64,
    pub bpm_source: Option<f64>,
    pub bpm_target: Option<f64>,

    pub preserve_pitch: bool,
    pub pitch_shift_semitones: f32,
    pub formant_preserve: bool,

    pub transient_preserve: bool,
    pub transient_sensitivity: f32,

    pub reverse: bool,
    pub normalize_gain: bool,

    pub fade_in_ms: f32,
    pub fade_out_ms: f32,

    pub gain_db: f32,
    pub pan: f32,

    pub dirty: bool,

    /// Warp markers (spec §2 Warp). Stored, rendered, and mapped into
    /// per-segment playback stretch by the engine.
    pub warp_markers: Vec<WarpMarker>,

    /// Non-destructive clip gain envelope. The source file is never rewritten;
    /// the editor and future render path evaluate these points relative to the
    /// clip gain.
    pub gain_envelope: ClipEnvelope,

    /// Adaptive de-noise amount. `0` is bypass; higher values apply stronger
    /// reduction to steady low-level noise during playback and bounce.
    pub denoise_amount: f32,

    /// Non-destructive channel transform. Identity is Stereo.
    pub channel_transform: u8,
    pub dc_remove: bool,
    pub dc_left: f32,
    pub dc_right: f32,
    pub dehum_hz: f32,
    pub dehum_harmonics: u8,
    pub dehum_reduction_db: f32,
}

impl Default for AudioClipStretchState {
    fn default() -> Self {
        // Backward-compat load defaults: a clip with no stretch info is an
        // un-stretched clip at 1.0× with pitch preservation disabled.
        Self {
            mode: StretchMode::Off,
            algorithm: StretchAlgorithm::Auto,
            original_sample_rate: 0,
            project_sample_rate: 0,
            original_duration_samples: 0,
            source_start_samples: 0,
            source_end_samples: 0,
            clip_timeline_start_beats: 0.0,
            clip_timeline_duration_beats: 0.0,
            stretch_ratio: 1.0,
            bpm_source: None,
            bpm_target: None,
            preserve_pitch: false,
            pitch_shift_semitones: 0.0,
            formant_preserve: false,
            transient_preserve: true,
            transient_sensitivity: 0.5,
            reverse: false,
            normalize_gain: false,
            fade_in_ms: 0.0,
            fade_out_ms: 0.0,
            gain_db: 0.0,
            pan: 0.0,
            dirty: false,
            warp_markers: Vec::new(),
            gain_envelope: ClipEnvelope::default(),
            denoise_amount: 0.0,
            channel_transform: 0,
            dc_remove: false,
            dc_left: 0.0,
            dc_right: 0.0,
            dehum_hz: 0.0,
            dehum_harmonics: 0,
            dehum_reduction_db: 0.0,
        }
    }
}

impl AudioClipStretchState {
    /// Clamp bounds for a sane, non-zero, finite stretch ratio.
    pub const MIN_RATIO: f64 = 0.05;
    pub const MAX_RATIO: f64 = 20.0;
    /// Hard ceiling for project-file and runtime marker lists.
    pub const MAX_WARP_MARKERS: usize = 2048;

    /// Normalize persisted or bridged values before they are used by the
    /// timeline or audio engine. This is deliberately idempotent so callers can
    /// apply it at every state boundary without changing valid projects.
    pub fn sanitize_in_place(&mut self) {
        self.stretch_ratio = if self.stretch_ratio.is_finite() {
            self.stretch_ratio.clamp(Self::MIN_RATIO, Self::MAX_RATIO)
        } else {
            1.0
        };
        self.bpm_source = self
            .bpm_source
            .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
            .map(|bpm| bpm.clamp(1.0, 999.0));
        self.bpm_target = self
            .bpm_target
            .filter(|bpm| bpm.is_finite() && *bpm > 0.0)
            .map(|bpm| bpm.clamp(1.0, 999.0));
        self.pitch_shift_semitones = if self.pitch_shift_semitones.is_finite() {
            self.pitch_shift_semitones.clamp(-48.0, 48.0)
        } else {
            0.0
        };
        self.transient_sensitivity = if self.transient_sensitivity.is_finite() {
            self.transient_sensitivity.clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.fade_in_ms = if self.fade_in_ms.is_finite() {
            self.fade_in_ms.max(0.0)
        } else {
            0.0
        };
        self.fade_out_ms = if self.fade_out_ms.is_finite() {
            self.fade_out_ms.max(0.0)
        } else {
            0.0
        };
        self.gain_db = if self.gain_db.is_finite() {
            self.gain_db.clamp(-120.0, 24.0)
        } else {
            0.0
        };
        self.denoise_amount = if self.denoise_amount.is_finite() {
            self.denoise_amount.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.pan = if self.pan.is_finite() {
            self.pan.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        self.warp_markers
            .retain(|marker| marker.timeline_beat.is_finite() && marker.timeline_beat >= 0.0);
        self.warp_markers.sort_by(|a, b| {
            a.id.cmp(&b.id)
                .then_with(|| a.timeline_beat.total_cmp(&b.timeline_beat))
        });
        self.warp_markers.dedup_by(|a, b| a.id == b.id);
        self.warp_markers.sort_by(|a, b| {
            a.timeline_beat
                .total_cmp(&b.timeline_beat)
                .then_with(|| a.id.cmp(&b.id))
        });
        self.warp_markers.truncate(Self::MAX_WARP_MARKERS);
        self.gain_envelope.sanitize_in_place();
    }

    // ── Pure math helpers (no clamping — exact for tests/spec §18) ──────────

    /// `100% → 1.0`, `200% → 2.0`, `50% → 0.5`.
    pub fn ratio_from_percent(percent: f64) -> f64 {
        percent / 100.0
    }

    /// Inverse of [`ratio_from_percent`]. `1.0 → 100%`, `2.0 → 200%`.
    pub fn percent_from_ratio(ratio: f64) -> f64 {
        ratio * 100.0
    }

    /// TempoSync ratio = `source_bpm / project_bpm` (spec §2 TempoSync). A
    /// non-positive project tempo degrades to `1.0` rather than dividing by zero.
    pub fn source_bpm_to_project_bpm_ratio(source_bpm: f64, project_bpm: f64) -> f64 {
        if project_bpm.abs() < f64::EPSILON {
            1.0
        } else {
            source_bpm / project_bpm
        }
    }

    /// Pitch multiplier from semitones (+ optional fine cents): `2^((semi + cents/100) / 12)`.
    pub fn pitch_ratio_from_semitones(semitones: f32) -> f64 {
        SphereAudioProcessor::semitone_to_pitch_ratio(semitones, 0.0) as f64
    }

    /// Pitch multiplier from separate semitone and cent controls.
    pub fn pitch_ratio_from_semi_and_cents(semitones: f32, cents: f32) -> f64 {
        SphereAudioProcessor::semitone_to_pitch_ratio(semitones, cents) as f64
    }

    /// Decompose the stored semitone shift into whole semitones and fine cents.
    pub fn pitch_semi_and_cents(&self) -> (f32, f32) {
        let semi = self.pitch_shift_semitones.trunc();
        let cents = (self.pitch_shift_semitones - semi) * 100.0;
        (semi, cents)
    }

    pub fn set_pitch_semi_and_cents(&mut self, semitones: f32, cents: f32) {
        let combined = (semitones + cents / 100.0).clamp(-48.0, 48.0);
        if (self.pitch_shift_semitones - combined).abs() > f32::EPSILON {
            self.pitch_shift_semitones = combined;
            self.dirty = true;
        }
    }

    pub fn reset_pitch(&mut self) {
        self.pitch_shift_semitones = 0.0;
        self.dirty = true;
    }

    // ── Derived getters ────────────────────────────────────────────────────

    /// Rate of the sample positions this state stores (`source_start_samples`,
    /// `source_end_samples`, warp `source_sample`): the source file's own rate.
    ///
    /// `project_sample_rate` is only a fallback for a clip whose file has not
    /// been decoded yet. Taking the larger of the two, as several call sites
    /// used to, turns a 44.1 kHz file in a 48 kHz project 8% short everywhere
    /// the UI converts its window to seconds, while the engine — which reads
    /// the file at its real rate — plays it at full length. `0` = unknown.
    pub fn source_sample_rate(&self) -> u32 {
        if self.original_sample_rate > 0 {
            self.original_sample_rate
        } else {
            self.project_sample_rate
        }
    }

    /// Current stretch as a percentage (`stretch_ratio * 100`).
    pub fn stretch_percent(&self) -> f64 {
        Self::percent_from_ratio(self.stretch_ratio)
    }

    /// Length of the active source window in samples (stored trim only).
    pub fn source_len_samples(&self) -> u64 {
        self.source_end_samples
            .saturating_sub(self.source_start_samples)
    }

    /// Resolve the source-sample window for playback, waveform, and offline
    /// analysis. When trim metadata was never written (`source_end <= source_start`),
    /// falls back to `original_duration_samples` or the full decoded file length.
    pub fn resolved_source_trim_range(&self, total_source_frames: u64) -> (u64, u64) {
        let total = total_source_frames.max(self.original_duration_samples);
        let start = self.source_start_samples.min(total);
        let end = if self.source_end_samples > start {
            self.source_end_samples.min(total)
        } else if self.original_duration_samples > start {
            self.original_duration_samples.min(total)
        } else {
            total
        };
        (start, end.max(start))
    }

    /// Effective trimmed source length once `total_source_frames` is known.
    pub fn resolved_source_len_samples(&self, total_source_frames: u64) -> u64 {
        let (start, end) = self.resolved_source_trim_range(total_source_frames);
        end.saturating_sub(start)
    }

    /// Ratio actually applied to playback timing. `Off` is always `1.0`
    /// regardless of the stored `stretch_ratio`.
    pub fn effective_ratio(&self) -> f64 {
        match self.mode {
            StretchMode::Off => 1.0,
            _ => self.stretch_ratio,
        }
    }

    /// Convert the native persistent clip state into the canonical
    /// SphereAudioProcessor params used by playback/export. Derived values stay
    /// owned by SphereAudioProcessor; this method only maps UI enum/storage names.
    pub fn to_sphere_stretch_params(
        &self,
        project_bpm: f64,
    ) -> SphereAudioProcessor::StretchParams {
        let preserve_pitch = self.keeps_pitch_engine();
        // A transpose only exists where pitch is decoupled from speed. On a
        // tape-style clip the read rate already *is* the pitch, and folding a
        // second factor into it made the engine read past (or stop short of)
        // the trimmed source window, because the clip length is derived from
        // the time ratio alone.
        let transposed = self.pitch_shift_semitones.abs() > 1.0e-4;
        let pitch_ratio = if preserve_pitch || (self.mode == StretchMode::Off && transposed) {
            Self::pitch_ratio_from_semitones(self.pitch_shift_semitones) as f32
        } else {
            1.0
        };
        let (mode, time_ratio) = match self.mode {
            // "Off" is about timing. A transposed Off clip still needs the
            // pitch-preserving processor, at exactly 1:1 time.
            StretchMode::Off if transposed => (SphereAudioProcessor::StretchMode::Manual, 1.0),
            StretchMode::Off => (SphereAudioProcessor::StretchMode::Off, 1.0),
            StretchMode::Resample | StretchMode::Manual => (
                SphereAudioProcessor::StretchMode::Manual,
                self.stretch_ratio as f32,
            ),
            StretchMode::TempoSync => (
                SphereAudioProcessor::StretchMode::TempoSync,
                self.stretch_ratio as f32,
            ),
            StretchMode::Warp => (
                SphereAudioProcessor::StretchMode::Warp,
                self.stretch_ratio as f32,
            ),
        };
        let preserve_pitch = preserve_pitch || (self.mode == StretchMode::Off && transposed);
        let algorithm = if mode == SphereAudioProcessor::StretchMode::Off {
            SphereAudioProcessor::StretchAlgorithm::Off
        } else if preserve_pitch {
            SphereAudioProcessor::StretchAlgorithm::PreservePitch
        } else {
            SphereAudioProcessor::StretchAlgorithm::RePitch
        };
        // Tempo Sync follows the project tempo, always. The stored
        // `bpm_target` used to win here, which froze a synced clip at whatever
        // tempo it was fitted under while the Inspector claimed it followed the
        // project.
        let target_bpm = match self.mode {
            StretchMode::TempoSync => Some(project_bpm as f32),
            _ => self.bpm_target.or(Some(project_bpm)).map(|v| v as f32),
        };

        SphereAudioProcessor::StretchParams {
            mode,
            algorithm,
            time_ratio,
            pitch_ratio,
            source_bpm: self.bpm_source.map(|v| v as f32),
            target_bpm,
            preserve_pitch,
            quality: match self.algorithm {
                StretchAlgorithm::ResampleOnly => 0.35,
                StretchAlgorithm::ElastiqueLike => 1.0,
                _ => 0.75,
            },
        }
    }

    /// Whether the engine renders this clip through the pitch-preserving
    /// processor because of its timing mode (a transposed `Off` clip is handled
    /// separately in [`Self::to_sphere_stretch_params`]).
    fn keeps_pitch_engine(&self) -> bool {
        matches!(
            self.mode,
            StretchMode::Manual | StretchMode::TempoSync | StretchMode::Warp
        ) && self.preserve_pitch
            && !matches!(self.algorithm, StretchAlgorithm::ResampleOnly)
    }

    // ── Inspector intent ───────────────────────────────────────────────────

    /// The timing choice this state represents. See [`StretchTiming`].
    pub fn timing(&self) -> StretchTiming {
        match self.mode {
            StretchMode::Off => StretchTiming::Off,
            StretchMode::Resample | StretchMode::Manual => StretchTiming::Speed,
            StretchMode::TempoSync => StretchTiming::Tempo,
            StretchMode::Warp => StretchTiming::Warp,
        }
    }

    /// Whether pitch stays put when the speed changes. `false` is tape-style:
    /// playing faster plays higher. An `Off` clip does not change speed, so it
    /// trivially keeps its pitch.
    pub fn keeps_pitch(&self) -> bool {
        match self.mode {
            StretchMode::Off => true,
            _ => self.keeps_pitch_engine(),
        }
    }

    /// Whether the transpose control applies to this clip. It does wherever
    /// pitch is decoupled from speed; on a tape-style clip the speed *is* the
    /// pitch.
    pub fn transpose_available(&self) -> bool {
        self.keeps_pitch()
    }

    /// Store `keep` as the pitch choice for the current timing.
    fn apply_keep_pitch(&mut self, keep: bool) {
        match self.mode {
            StretchMode::Off => {}
            StretchMode::Resample | StretchMode::Manual => {
                self.mode = if keep {
                    StretchMode::Manual
                } else {
                    StretchMode::Resample
                };
            }
            StretchMode::TempoSync | StretchMode::Warp => {}
        }
        if self.mode != StretchMode::Off {
            self.preserve_pitch = keep;
            self.algorithm = if keep {
                StretchAlgorithm::PhaseVocoder
            } else {
                StretchAlgorithm::ResampleOnly
            };
        }
    }

    /// Next state for a timing choice. The clip keeps sounding the same length
    /// across the switch wherever the new timing allows it: Speed and Warp
    /// start from the ratio the clip was actually playing at, so choosing
    /// "Speed" on a tempo-synced clip does not make it jump.
    pub fn with_timing(&self, timing: StretchTiming, project_bpm: f64) -> Self {
        let mut next = self.clone();
        if next.timing() == timing {
            return next;
        }
        let keep = self.keeps_pitch();
        let playing_ratio = self.effective_time_ratio(project_bpm);
        match timing {
            StretchTiming::Off => {
                next.mode = StretchMode::Off;
                next.algorithm = StretchAlgorithm::Auto;
            }
            StretchTiming::Speed => {
                next.mode = StretchMode::Manual;
                next.set_stretch_ratio(playing_ratio);
                next.apply_keep_pitch(keep);
            }
            StretchTiming::Tempo => {
                next.mode = StretchMode::TempoSync;
                next.apply_keep_pitch(keep);
            }
            StretchTiming::Warp => {
                next.mode = StretchMode::Warp;
                next.set_stretch_ratio(playing_ratio);
                next.apply_keep_pitch(keep);
            }
        }
        // A transpose set on a tape clip was inert; it must not suddenly sound
        // when the clip moves to a pitch-keeping timing.
        if !keep && next.keeps_pitch() {
            next.pitch_shift_semitones = 0.0;
        }
        next.clip_timeline_duration_beats = 0.0;
        next.dirty = true;
        next
    }

    /// Next state with the pitch choice changed. No-op for `Off`.
    pub fn with_keep_pitch(&self, keep: bool) -> Self {
        let mut next = self.clone();
        if next.mode == StretchMode::Off || next.keeps_pitch() == keep {
            return next;
        }
        next.apply_keep_pitch(keep);
        if !keep {
            next.pitch_shift_semitones = 0.0;
        }
        next.dirty = true;
        next
    }

    /// Effective playback duration of the source window after stretching, in
    /// samples. `ratio 2.0` → twice as long; `ratio 0.5` → half (spec §2 Manual).
    pub fn effective_duration_samples(&self) -> u64 {
        self.effective_duration_samples_for_project_bpm(self.bpm_target.unwrap_or(120.0))
    }

    /// Effective playback duration of the source window after stretching, resolving
    /// Tempo Sync against the supplied project tempo.
    pub fn effective_duration_samples_for_project_bpm(&self, project_bpm: f64) -> u64 {
        SphereAudioProcessor::stretched_duration_samples(
            self.source_len_samples(),
            &self.to_sphere_stretch_params(project_bpm),
            Some(project_bpm as f32),
        )
    }

    /// Wall-clock length this clip plays for at `project_bpm`, in seconds, or
    /// `None` when the source window has not been decoded yet.
    ///
    /// The one place a clip's real length is computed. The timeline draws from
    /// it and [`TimelineState::reconcile_audio_clip_lengths`] derives
    /// `duration_beats` from it, so the picture and the model cannot disagree
    /// about how long a clip is.
    pub fn played_seconds_for_project_bpm(&self, project_bpm: f64) -> Option<f64> {
        let rate = self.source_sample_rate();
        if rate == 0 || self.source_len_samples() == 0 {
            return None;
        }
        let played = self.effective_duration_samples_for_project_bpm(project_bpm);
        if played == 0 {
            return None;
        }
        Some(played as f64 / rate as f64)
    }

    /// Whether the project tempo owns this clip's *bar count* rather than its
    /// wall-clock length. Tempo Sync and Warp are defined in beats; every other
    /// mode is defined in seconds.
    pub fn follows_project_tempo(&self) -> bool {
        matches!(self.mode, StretchMode::TempoSync | StretchMode::Warp)
    }

    /// Time-stretch ratio actually used for playback / clip length, resolving
    /// `TempoSync` against the project tempo. `Off` → `1.0`; `Warp` falls back to
    /// the stored manual ratio; `TempoSync` with no source BPM → `1.0`.
    ///
    /// This is the single source of truth shared by the inspector (clip-length
    /// coupling), the engine snapshot (`speed_ratio`), and tests, so visual and
    /// audible length never diverge.
    pub fn effective_time_ratio(&self, project_bpm: f64) -> f64 {
        SphereAudioProcessor::effective_time_ratio(
            &self.to_sphere_stretch_params(project_bpm),
            Some(project_bpm as f32),
        ) as f64
    }

    /// Source-read rate (source samples consumed per output sample) for the
    /// resample DSP path: folds the time-stretch reciprocal with the explicit
    /// pitch shift — `speed = pitch_ratio / time_ratio`. Clamped to the engine's
    /// accepted `speed_ratio` range.
    ///
    /// `preserve_pitch` affects backend selection, not this RePitch read-rate
    /// helper. PreservePitch rendering is resolved by `SphereAudioProcessor`.
    pub fn resample_speed_ratio(&self, project_bpm: f64) -> f64 {
        SphereAudioProcessor::source_read_rate_for_repitch(
            &self.to_sphere_stretch_params(project_bpm),
            Some(project_bpm as f32),
        ) as f64
    }

    /// Whether changing clip length changes pitch (tape-style), given the mode
    /// and `preserve_pitch` (spec §6 behaviour matrix).
    pub fn pitch_linked_to_duration(&self) -> bool {
        match self.mode {
            StretchMode::Resample => true,
            StretchMode::Manual | StretchMode::TempoSync | StretchMode::Warp => {
                !self.preserve_pitch
            }
            StretchMode::Off => false,
        }
    }

    /// Net pitch multiplier applied to playback, combining the explicit semitone
    /// shift with the duration-linked component when pitch is not preserved.
    pub fn playback_pitch_ratio(&self) -> f64 {
        let semis = Self::pitch_ratio_from_semitones(self.pitch_shift_semitones);
        if self.pitch_linked_to_duration() {
            let r = self.effective_ratio();
            if r.abs() < f64::EPSILON {
                semis
            } else {
                // Stretching longer (ratio > 1) reads the source slower → lower
                // pitch, hence the reciprocal.
                semis / r
            }
        } else {
            semis
        }
    }

    // ── Mutators (clamped; mark dirty) ─────────────────────────────────────

    /// Set the stretch ratio, clamped to `[MIN_RATIO, MAX_RATIO]`.
    pub fn set_stretch_ratio(&mut self, ratio: f64) {
        let clamped = if ratio.is_finite() {
            ratio.clamp(Self::MIN_RATIO, Self::MAX_RATIO)
        } else {
            1.0
        };
        if (self.stretch_ratio - clamped).abs() > f64::EPSILON {
            self.stretch_ratio = clamped;
            self.dirty = true;
        }
    }

    /// Set the stretch by percent (`200%` → ratio `2.0`).
    pub fn set_stretch_percent(&mut self, percent: f64) {
        self.set_stretch_ratio(Self::ratio_from_percent(percent));
    }

    /// Trim adjusts the active source window but **never** the stretch ratio
    /// (spec §7 — normal edge drag changes the visible source range only).
    pub fn apply_trim(&mut self, source_start_samples: u64, source_end_samples: u64) {
        self.source_start_samples = source_start_samples;
        self.source_end_samples = source_end_samples.max(source_start_samples);
        self.dirty = true;
        // stretch_ratio intentionally left unchanged.
    }

    /// Stretch-drag sets a new timeline length (in samples) for the same source
    /// window and recomputes the ratio so the window fills it (spec §7 — stretch
    /// edge drag keeps the source range, updates `stretch_ratio`).
    pub fn apply_stretch_to_timeline_samples(&mut self, new_timeline_len_samples: u64) {
        let src = self.source_len_samples();
        if src > 0 {
            self.set_stretch_ratio(new_timeline_len_samples as f64 / src as f64);
        }
    }

    /// Apply constant-tempo sync against a project tempo: stores the target tempo
    /// and sets the ratio from the source/target BPM pair.
    pub fn apply_tempo_sync(&mut self, project_bpm: f64) {
        self.bpm_target = Some(project_bpm);
        if let Some(source_bpm) = self.bpm_source {
            self.set_stretch_ratio(Self::source_bpm_to_project_bpm_ratio(
                source_bpm,
                project_bpm,
            ));
        }
    }

    pub fn fit_to_project_tempo(&mut self, project_bpm: f64) -> bool {
        let Some(source_bpm) = self.bpm_source else {
            return false;
        };
        self.mode = StretchMode::TempoSync;
        self.bpm_target = Some(project_bpm);
        self.clip_timeline_duration_beats = 0.0;
        self.set_stretch_ratio(Self::source_bpm_to_project_bpm_ratio(
            source_bpm,
            project_bpm,
        ));
        self.dirty = true;
        true
    }

    pub fn fit_to_timeline_beats(&mut self, timeline_beats: f64, project_bpm: f64) -> bool {
        let source_len = self.source_len_samples();
        let sample_rate = self.source_sample_rate().max(1) as f64;
        if source_len == 0 || timeline_beats <= 0.0 || project_bpm <= 0.0 {
            return false;
        }
        let target_samples = timeline_beats * (60.0 / project_bpm) * sample_rate;
        if !target_samples.is_finite() || target_samples <= 0.0 {
            return false;
        }
        self.mode = StretchMode::Manual;
        self.clip_timeline_duration_beats = timeline_beats;
        self.set_stretch_ratio(target_samples / source_len as f64);
        self.dirty = true;
        true
    }

    pub fn reset_stretch_defaults(&mut self) {
        self.mode = StretchMode::Off;
        self.algorithm = StretchAlgorithm::Auto;
        self.stretch_ratio = 1.0;
        self.clip_timeline_duration_beats = 0.0;
        self.bpm_source = None;
        self.bpm_target = None;
        self.preserve_pitch = false;
        self.reset_pitch();
        self.formant_preserve = false;
        self.transient_preserve = false;
        self.transient_sensitivity = 0.5;
        self.dirty = true;
        self.warp_markers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(frames: u64, rate: u32) -> AudioClipStretchState {
        AudioClipStretchState {
            original_sample_rate: rate,
            project_sample_rate: rate,
            original_duration_samples: frames,
            source_start_samples: 0,
            source_end_samples: frames,
            ..AudioClipStretchState::default()
        }
    }

    #[test]
    fn tempo_sync_follows_the_project_tempo_not_the_tempo_it_was_fitted_at() {
        let mut s = decoded(48_000, 48_000).with_timing(StretchTiming::Tempo, 120.0);
        s.bpm_source = Some(120.0);
        // Fitting used to store the target; it must not freeze the clip.
        assert!(s.fit_to_project_tempo(120.0));
        approx(s.effective_time_ratio(120.0), 1.0);
        approx(s.effective_time_ratio(60.0), 2.0);
        approx(s.effective_time_ratio(240.0), 0.5);
    }

    #[test]
    fn tape_clips_never_carry_a_transpose_into_the_engine() {
        // A transpose on a tape clip used to change the read rate but not the
        // length, so the engine read past the trimmed window.
        let mut s = decoded(48_000, 48_000)
            .with_timing(StretchTiming::Speed, 120.0)
            .with_keep_pitch(false);
        s.pitch_shift_semitones = 12.0;
        let params = s.to_sphere_stretch_params(120.0);
        assert!(!params.preserve_pitch);
        assert!((params.pitch_ratio - 1.0).abs() < 1e-6);
        assert!(!s.transpose_available());
    }

    #[test]
    fn an_unstretched_clip_can_be_transposed() {
        let mut s = decoded(48_000, 48_000);
        s.set_pitch_semi_and_cents(3.0, 0.0);
        assert_eq!(s.timing(), StretchTiming::Off);
        assert!(s.transpose_available());
        let params = s.to_sphere_stretch_params(120.0);
        assert!(params.preserve_pitch);
        assert_eq!(
            params.algorithm,
            SphereAudioProcessor::StretchAlgorithm::PreservePitch
        );
        approx(s.effective_time_ratio(120.0), 1.0);
        assert!(params.pitch_ratio > 1.18 && params.pitch_ratio < 1.19);
    }

    #[test]
    fn warp_keeps_pitch_through_the_engine_params() {
        let s = decoded(48_000, 48_000).with_timing(StretchTiming::Warp, 120.0);
        assert!(s.keeps_pitch());
        assert!(s.to_sphere_stretch_params(120.0).preserve_pitch);
        let tape = s.with_keep_pitch(false);
        assert!(!tape.to_sphere_stretch_params(120.0).preserve_pitch);
        assert_eq!(tape.timing(), StretchTiming::Warp);
    }

    #[test]
    fn played_length_uses_the_file_rate() {
        // A 44.1 kHz file in a 48 kHz project is still one second long.
        let s = AudioClipStretchState {
            project_sample_rate: 48_000,
            ..decoded(44_100, 44_100)
        };
        approx(s.played_seconds_for_project_bpm(120.0).unwrap(), 1.0);
    }

    #[test]
    fn switching_timing_keeps_the_playing_length() {
        let mut s = decoded(48_000, 48_000).with_timing(StretchTiming::Tempo, 120.0);
        s.bpm_source = Some(90.0);
        let tempo_ratio = s.effective_time_ratio(120.0);
        let speed = s.with_timing(StretchTiming::Speed, 120.0);
        assert_eq!(speed.timing(), StretchTiming::Speed);
        approx(speed.effective_time_ratio(120.0), tempo_ratio);
        assert!(speed.keeps_pitch());
        let off = speed.with_timing(StretchTiming::Off, 120.0);
        approx(off.effective_time_ratio(120.0), 1.0);
    }

    #[test]
    fn speed_pitch_choice_maps_to_the_stored_modes() {
        let keep = decoded(48_000, 48_000).with_timing(StretchTiming::Speed, 120.0);
        assert_eq!(keep.mode, StretchMode::Manual);
        assert!(keep.keeps_pitch());
        let tape = keep.with_keep_pitch(false);
        assert_eq!(tape.mode, StretchMode::Resample);
        assert_eq!(tape.algorithm, StretchAlgorithm::ResampleOnly);
        assert!(!tape.keeps_pitch());
        assert_eq!(tape.timing(), StretchTiming::Speed);
        let back = tape.with_keep_pitch(true);
        assert_eq!(back.mode, StretchMode::Manual);
        assert!(back.keeps_pitch());
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    /// A Manual-mode clip over a fixed source window, for duration/ratio tests.
    fn manual_clip(source_len: u64) -> AudioClipStretchState {
        AudioClipStretchState {
            mode: StretchMode::Manual,
            source_start_samples: 0,
            source_end_samples: source_len,
            ..AudioClipStretchState::default()
        }
    }

    #[test]
    fn resolved_source_trim_range_defaults_to_full_file() {
        let s = AudioClipStretchState::default();
        let (start, end) = s.resolved_source_trim_range(48_000);
        assert_eq!(start, 0);
        assert_eq!(end, 48_000);
        assert_eq!(s.resolved_source_len_samples(48_000), 48_000);
    }

    #[test]
    fn resolved_source_trim_range_honors_explicit_trim() {
        let s = AudioClipStretchState {
            source_start_samples: 1_000,
            source_end_samples: 9_000,
            ..AudioClipStretchState::default()
        };
        let (start, end) = s.resolved_source_trim_range(48_000);
        assert_eq!((start, end), (1_000, 9_000));
    }

    #[test]
    fn resolved_source_trim_range_uses_original_duration_when_end_missing() {
        let s = AudioClipStretchState {
            original_duration_samples: 24_000,
            ..AudioClipStretchState::default()
        };
        let (start, end) = s.resolved_source_trim_range(48_000);
        assert_eq!((start, end), (0, 24_000));
    }

    #[test]
    fn stretch_ratio_from_percent() {
        approx(AudioClipStretchState::ratio_from_percent(100.0), 1.0);
        approx(AudioClipStretchState::ratio_from_percent(200.0), 2.0);
        approx(AudioClipStretchState::ratio_from_percent(50.0), 0.5);
    }

    #[test]
    fn percent_from_ratio() {
        approx(AudioClipStretchState::percent_from_ratio(1.0), 100.0);
        approx(AudioClipStretchState::percent_from_ratio(2.0), 200.0);
        approx(AudioClipStretchState::percent_from_ratio(0.5), 50.0);
    }

    #[test]
    fn source_bpm_to_project_bpm_ratio() {
        // 120 BPM source loop in a 140 BPM project → ratio 120 / 140 (faster).
        approx(
            AudioClipStretchState::source_bpm_to_project_bpm_ratio(120.0, 140.0),
            120.0 / 140.0,
        );
        // Degenerate project tempo degrades to 1.0 rather than dividing by zero.
        approx(
            AudioClipStretchState::source_bpm_to_project_bpm_ratio(120.0, 0.0),
            1.0,
        );
    }

    #[test]
    fn pitch_ratio_from_semitones() {
        approx(AudioClipStretchState::pitch_ratio_from_semitones(0.0), 1.0);
        approx(AudioClipStretchState::pitch_ratio_from_semitones(12.0), 2.0);
        approx(
            AudioClipStretchState::pitch_ratio_from_semitones(-12.0),
            0.5,
        );
    }

    #[test]
    fn clip_duration_after_manual_stretch() {
        let mut s = manual_clip(1000);
        s.set_stretch_percent(200.0); // twice as long / slower
        approx(s.stretch_ratio, 2.0);
        assert_eq!(s.effective_duration_samples(), 2000);

        s.set_stretch_percent(50.0); // half length / faster
        approx(s.stretch_ratio, 0.5);
        assert_eq!(s.effective_duration_samples(), 500);
    }

    #[test]
    fn trim_does_not_change_stretch_ratio() {
        let mut s = manual_clip(1000);
        s.set_stretch_ratio(1.5);
        s.apply_trim(100, 600);
        // The ratio is unchanged; only the source window moved (spec §7).
        approx(s.stretch_ratio, 1.5);
        assert_eq!(s.source_len_samples(), 500);
    }

    #[test]
    fn stretch_drag_does_change_stretch_ratio() {
        let mut s = manual_clip(1000);
        // Dragging the edge so the same 1000-sample window fills 1500 samples
        // of timeline → ratio 1.5 (spec §7).
        s.apply_stretch_to_timeline_samples(1500);
        approx(s.stretch_ratio, 1.5);
        assert_eq!(s.source_len_samples(), 1000);
    }

    #[test]
    fn default_stretch_is_off_repitch_safe() {
        let s = AudioClipStretchState::default();
        assert_eq!(s.mode, StretchMode::Off);
        assert_eq!(s.algorithm, StretchAlgorithm::Auto);
        approx(s.stretch_ratio, 1.0);
        assert!(!s.preserve_pitch);
    }

    #[test]
    fn off_mode_ignores_ratio_for_effective_duration() {
        let mut s = manual_clip(1000);
        s.set_stretch_ratio(2.0);
        s.mode = StretchMode::Off;
        // Off always plays 1:1 regardless of the stored ratio.
        approx(s.effective_ratio(), 1.0);
        assert_eq!(s.effective_duration_samples(), 1000);
    }

    #[test]
    fn tempo_sync_duration_uses_project_bpm() {
        let mut s = manual_clip(48_000);
        s.mode = StretchMode::TempoSync;
        s.bpm_source = Some(120.0);
        assert_eq!(s.effective_duration_samples_for_project_bpm(60.0), 96_000);
        assert_eq!(s.effective_duration_samples_for_project_bpm(240.0), 24_000);
    }

    #[test]
    fn resample_links_pitch_to_duration() {
        let mut s = manual_clip(1000);
        s.mode = StretchMode::Resample;
        s.set_stretch_ratio(2.0); // twice as long → an octave down
        approx(s.playback_pitch_ratio(), 0.5);
    }

    #[test]
    fn preserve_pitch_decouples_pitch_from_duration() {
        let mut s = manual_clip(1000);
        s.preserve_pitch = true;
        s.set_stretch_ratio(2.0);
        s.pitch_shift_semitones = 12.0; // explicit +1 octave only
        approx(s.playback_pitch_ratio(), 2.0);
    }

    #[test]
    fn output_to_source_maps_stretched_halfway_to_source_quarter() {
        approx(
            clip_output_local_to_source_sample(500.0, 0, 1_000, 2.0, false),
            250.0,
        );
    }

    #[test]
    fn output_to_source_maps_compressed_faster_through_source() {
        approx(
            clip_output_local_to_source_sample(250.0, 0, 1_000, 0.5, false),
            500.0,
        );
    }

    #[test]
    fn output_to_source_reverse_starts_at_source_end() {
        approx(
            clip_output_local_to_source_sample(0.0, 0, 1_000, 2.0, true),
            1_000.0,
        );
        approx(
            clip_output_local_to_source_sample(500.0, 0, 1_000, 2.0, true),
            750.0,
        );
    }

    #[test]
    fn output_to_source_honors_trimmed_source_window() {
        approx(
            clip_output_local_to_source_sample(400.0, 100, 900, 2.0, false),
            300.0,
        );
    }

    #[test]
    fn warp_without_markers_uses_global_ratio() {
        approx(
            warp_timeline_beat_to_source_sample(500.0, 0, 1_000, 2.0, &[]),
            250.0,
        );
    }

    #[test]
    fn warp_with_two_markers_maps_linearly_between_them() {
        let markers = vec![
            WarpMarker {
                id: 1,
                source_sample: 100,
                timeline_beat: 1.0,
                locked: false,
            },
            WarpMarker {
                id: 2,
                source_sample: 900,
                timeline_beat: 5.0,
                locked: false,
            },
        ];
        approx(
            warp_timeline_beat_to_source_sample(3.0, 0, 1_000, 1.0, &markers),
            500.0,
        );
    }

    #[test]
    fn effective_time_ratio_resolves_modes() {
        let mut s = manual_clip(1000);
        s.set_stretch_ratio(1.5);
        approx(s.effective_time_ratio(120.0), 1.5);

        s.mode = StretchMode::Off;
        approx(s.effective_time_ratio(120.0), 1.0);

        s.mode = StretchMode::TempoSync;
        s.bpm_source = Some(120.0);
        approx(s.effective_time_ratio(140.0), 120.0 / 140.0);
        s.bpm_source = None;
        approx(s.effective_time_ratio(140.0), 1.0);
    }

    #[test]
    fn resample_speed_ratio_folds_time_and_pitch() {
        let mut s = manual_clip(1000);
        // ratio 2.0 (twice as long) → read source at half speed.
        s.set_stretch_ratio(2.0);
        approx(s.resample_speed_ratio(120.0), 0.5);
        // ratio 0.5 (half length) → read source twice as fast.
        s.set_stretch_ratio(0.5);
        approx(s.resample_speed_ratio(120.0), 2.0);
        // A tape clip's read rate is its speed and nothing else: a stored
        // transpose must not make it read faster than its length allows (it
        // would run past the trimmed window). Transpose needs Keep Pitch.
        s.set_stretch_ratio(1.0);
        s.pitch_shift_semitones = 12.0;
        approx(s.resample_speed_ratio(120.0), 1.0);
        // Off mode ignores the stored ratio for the time component.
        s.mode = StretchMode::Off;
        s.pitch_shift_semitones = 0.0;
        approx(s.resample_speed_ratio(120.0), 1.0);
    }

    #[test]
    fn set_stretch_ratio_clamps_to_bounds() {
        let mut s = manual_clip(1000);
        s.set_stretch_ratio(1000.0);
        approx(s.stretch_ratio, AudioClipStretchState::MAX_RATIO);
        s.set_stretch_ratio(0.0);
        approx(s.stretch_ratio, AudioClipStretchState::MIN_RATIO);
        s.set_stretch_ratio(f64::NAN);
        approx(s.stretch_ratio, 1.0);
    }

    #[test]
    fn sanitize_in_place_discards_invalid_marker_positions() {
        let mut s = AudioClipStretchState {
            stretch_ratio: f64::NAN,
            pitch_shift_semitones: f32::NAN,
            warp_markers: vec![
                WarpMarker {
                    id: 2,
                    source_sample: 200,
                    timeline_beat: 4.0,
                    locked: false,
                },
                WarpMarker {
                    id: 1,
                    source_sample: 100,
                    timeline_beat: f64::NAN,
                    locked: false,
                },
            ],
            ..AudioClipStretchState::default()
        };

        s.sanitize_in_place();

        approx(s.stretch_ratio, 1.0);
        assert_eq!(s.pitch_shift_semitones, 0.0);
        assert_eq!(s.warp_markers.len(), 1);
        assert_eq!(s.warp_markers[0].id, 2);
    }

    #[test]
    fn fit_to_project_tempo_requires_source_bpm() {
        let mut s = manual_clip(48_000);
        assert!(!s.fit_to_project_tempo(120.0));
        s.bpm_source = Some(120.0);
        assert!(s.fit_to_project_tempo(60.0));
        assert_eq!(s.mode, StretchMode::TempoSync);
        approx(s.stretch_ratio, 2.0);
    }

    #[test]
    fn fit_to_timeline_beats_uses_trimmed_source_length() {
        let mut s = manual_clip(48_000);
        s.project_sample_rate = 48_000;
        s.apply_trim(12_000, 36_000);
        assert!(s.fit_to_timeline_beats(2.0, 120.0));
        assert_eq!(s.mode, StretchMode::Manual);
        approx(s.stretch_ratio, 2.0);
    }

    #[test]
    fn reset_stretch_defaults_clears_active_fields() {
        let mut s = manual_clip(48_000);
        s.mode = StretchMode::TempoSync;
        s.algorithm = StretchAlgorithm::PhaseVocoder;
        s.set_stretch_ratio(2.0);
        s.bpm_source = Some(128.0);
        s.bpm_target = Some(120.0);
        s.preserve_pitch = true;
        s.pitch_shift_semitones = 3.0;
        s.formant_preserve = true;
        s.transient_preserve = true;
        s.reset_stretch_defaults();
        assert_eq!(s.mode, StretchMode::Off);
        assert_eq!(s.algorithm, StretchAlgorithm::Auto);
        approx(s.stretch_ratio, 1.0);
        assert_eq!(s.bpm_source, None);
        assert_eq!(s.bpm_target, None);
        assert!(!s.preserve_pitch);
        assert!(!s.formant_preserve);
        assert!(!s.transient_preserve);
    }

    #[test]
    fn detect_tempo_from_mono_finds_simple_pulse() {
        let sample_rate = 11_025.0;
        let seconds = 8.0;
        let mut samples = vec![0.0; (sample_rate * seconds) as usize];
        let beat = (sample_rate * 0.5) as usize;
        for i in (0..samples.len()).step_by(beat) {
            for j in 0..128 {
                if let Some(sample) = samples.get_mut(i + j) {
                    *sample = 1.0 - (j as f32 / 128.0);
                }
            }
        }
        let result =
            detect_tempo_from_mono(&samples, sample_rate, 60.0, 200.0, Some(120.0)).unwrap();
        assert!(
            (result.bpm - 120.0).abs() < 8.0,
            "expected ~120 BPM, got {:?}",
            result
        );
        assert!(result
            .alternatives
            .iter()
            .any(|b| (*b - 120.0).abs() < 10.0));
    }

    #[test]
    fn choose_musical_bpm_prefers_project_when_scores_close() {
        let candidates = vec![
            TempoCandidate {
                bpm: 107.67,
                confidence: 0.81,
                relation: TempoRelation::Raw,
            },
            TempoCandidate {
                bpm: 127.0,
                confidence: 0.77,
                relation: TempoRelation::ThreeHalves,
            },
        ];
        let result = choose_musical_bpm_candidate(&candidates, Some(127.0), 60.0, 200.0).unwrap();
        assert!((result.bpm - 127.0).abs() < 0.5);
    }

    #[test]
    fn choose_musical_bpm_keeps_best_raw_when_project_candidate_weak() {
        let candidates = vec![
            TempoCandidate {
                bpm: 107.0,
                confidence: 0.81,
                relation: TempoRelation::Raw,
            },
            TempoCandidate {
                bpm: 127.0,
                confidence: 0.40,
                relation: TempoRelation::ThreeHalves,
            },
        ];
        let result = choose_musical_bpm_candidate(&candidates, Some(127.0), 60.0, 200.0).unwrap();
        assert!((result.bpm - 107.0).abs() < 0.5);
    }

    #[test]
    fn choose_musical_bpm_doubles_slow_candidate_near_project() {
        let candidates = vec![
            TempoCandidate {
                bpm: 63.5,
                confidence: 0.70,
                relation: TempoRelation::Half,
            },
            TempoCandidate {
                bpm: 127.0,
                confidence: 0.68,
                relation: TempoRelation::Double,
            },
        ];
        let result = choose_musical_bpm_candidate(&candidates, Some(127.0), 60.0, 200.0).unwrap();
        assert!((result.bpm - 127.0).abs() < 1.0);
    }

    #[test]
    fn choose_musical_bpm_corrects_common_drift_via_six_fifths() {
        let candidates = vec![
            TempoCandidate {
                bpm: 107.67,
                confidence: 0.81,
                relation: TempoRelation::Raw,
            },
            TempoCandidate {
                bpm: 129.2,
                confidence: 0.76,
                relation: TempoRelation::ThreeHalves,
            },
        ];
        let result = choose_musical_bpm_candidate(&candidates, Some(127.0), 60.0, 200.0).unwrap();
        assert!((result.bpm - 129.2).abs() < 1.0 || (result.bpm - 127.0).abs() < 2.0);
    }

    #[test]
    fn fit_project_tempo_ratio_examples() {
        approx(
            AudioClipStretchState::source_bpm_to_project_bpm_ratio(127.0, 127.0),
            1.0,
        );
        approx(
            AudioClipStretchState::source_bpm_to_project_bpm_ratio(140.0, 127.0),
            140.0 / 127.0,
        );
        approx(
            AudioClipStretchState::source_bpm_to_project_bpm_ratio(100.0, 127.0),
            100.0 / 127.0,
        );
    }

    // ── Auto Find BPM fixes (spec Fix 5/6/8, acceptance tests) ──────────────

    #[test]
    fn project_prior_promotes_strong_near_candidate_over_raw_top() {
        // Fix 10 case 1: raw top 124 @ 1.0 vs 118 @ 0.88, project 120. 118 is
        // within ±3% of project and within 20% of the top score, so it outranks
        // 124 even though 124 scored slightly higher raw.
        let candidates = vec![
            TempoCandidate {
                bpm: 124.0,
                confidence: 1.0,
                relation: TempoRelation::Raw,
            },
            TempoCandidate {
                bpm: 118.0,
                confidence: 0.88,
                relation: TempoRelation::Raw,
            },
        ];
        let result = choose_musical_bpm_candidate(&candidates, Some(120.0), 60.0, 200.0).unwrap();
        assert!(
            (result.bpm - 118.0).abs() < 0.5,
            "expected ~118 promoted, got {result:?}"
        );
    }

    #[test]
    fn project_prior_keeps_raw_top_when_near_candidate_is_weak() {
        // Fix 10 case 2: raw top 124 @ 1.0 vs 118 @ 0.40, project 120. 118 is
        // below the 20% margin, so the confident 124 stays selected.
        let candidates = vec![
            TempoCandidate {
                bpm: 124.0,
                confidence: 1.0,
                relation: TempoRelation::Raw,
            },
            TempoCandidate {
                bpm: 118.0,
                confidence: 0.40,
                relation: TempoRelation::Raw,
            },
        ];
        let result = choose_musical_bpm_candidate(&candidates, Some(120.0), 60.0, 200.0).unwrap();
        assert!(
            (result.bpm - 124.0).abs() < 0.5,
            "expected 124 retained, got {result:?}"
        );
    }

    #[test]
    fn project_prior_bonus_peaks_at_project_and_decays() {
        let exact = project_prior_bonus(120.0, 120.0);
        let near = project_prior_bonus(118.0, 120.0);
        let octave = project_prior_bonus(60.0, 120.0);
        approx(exact as f64, TEMPO_PROJECT_PRIOR_WEIGHT as f64);
        assert!(near < exact && near > octave);
        assert!(
            octave < 0.001,
            "an octave away should be negligible: {octave}"
        );
    }

    #[test]
    fn manual_source_bpm_drives_fit_project_ratio() {
        // Fix 10 case 4: user types 118, project 120 → ratio 118/120 = 0.98333.
        let mut s = manual_clip(48_000);
        s.bpm_source = Some(118.0);
        assert!(s.fit_to_project_tempo(120.0));
        approx(s.stretch_ratio, 118.0 / 120.0);
        approx(s.stretch_ratio, 0.983_333_333_333_333_3);
    }

    #[test]
    fn alternatives_always_surface_project_neighbourhood() {
        // Fix 10 case 5 / Fix 6: with a scan peaking at 118 and project 120, the
        // alternatives must include both the true tempo (118) and the project (120).
        let scan = TempoScan {
            bpms: vec![110.0, 114.0, 118.0, 120.0, 122.0, 126.0, 140.0],
            scores: vec![0.30, 0.60, 1.00, 0.55, 0.50, 0.20, 0.40],
            salience: 0.7,
        };
        let candidates = vec![TempoCandidate {
            bpm: 118.0,
            confidence: 1.0,
            relation: TempoRelation::Raw,
        }];
        let alts = build_tempo_alternatives(118.0, &candidates, &scan, Some(120.0), 60.0, 200.0);
        assert!(
            alts.iter().any(|b| (*b - 118.0).abs() < 1.0),
            "alternatives should include the detected tempo 118: {alts:?}"
        );
        assert!(
            alts.iter().any(|b| (*b - 120.0).abs() < 1.0),
            "alternatives should include the project tempo 120: {alts:?}"
        );
    }

    #[test]
    fn detect_118_pulse_against_120_project_surfaces_real_tempo() {
        // End-to-end: a clean 118 BPM pulse in a 120 BPM project must either be
        // detected near 118 or expose 118 and 120 as alternatives (spec acceptance).
        let sample_rate = 11_025.0;
        let seconds = 18.0;
        let mut samples = vec![0.0; (sample_rate * seconds) as usize];
        let beat = (sample_rate * 60.0 / 118.0) as usize; // 118 BPM
        for i in (0..samples.len()).step_by(beat) {
            for j in 0..128 {
                if let Some(sample) = samples.get_mut(i + j) {
                    *sample = 1.0 - (j as f32 / 128.0);
                }
            }
        }
        let result =
            detect_tempo_from_mono(&samples, sample_rate, 60.0, 200.0, Some(120.0)).unwrap();
        let near_118 = (result.bpm - 118.0).abs() < 4.0
            || result.alternatives.iter().any(|b| (*b - 118.0).abs() < 3.0);
        let has_project = result.alternatives.iter().any(|b| (*b - 120.0).abs() < 2.0);
        assert!(near_118, "expected 118 detected or offered: {result:?}");
        assert!(
            has_project,
            "expected project 120 among alternatives: {result:?}"
        );
    }
}

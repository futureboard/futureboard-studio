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
            // Fitted to the tempo: the clip plays its own beats on the
            // project's, wherever the tempo goes.
            StretchTiming::Tempo => "Fit",
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
/// Clips up to this many beats long are read as loops. See
/// [`whole_bar_tempo`].
const TEMPO_LOOP_MAX_BEATS: f32 = 128.0;
/// Furthest a loop's measured tempo is moved to make it a whole number of
/// bars. The measurement itself is good to about a tenth of a BPM.
const TEMPO_LOOP_SNAP_BPM: f32 = 0.3;

/// The tempo that makes a clip of `seconds` exactly a whole number of 4/4
/// bars, when it is within [`TEMPO_LOOP_SNAP_BPM`] of the measured `bpm`.
///
/// A loop is cut on its bar lines, so its length knows its tempo more
/// precisely than any onset analysis: an 8-bar loop at 123.37 BPM is 15.563
/// seconds, and a tempo a tenth of a BPM off drifts a sixteenth over the
/// loop. Only short clips are read this way — across a whole song the bar
/// counts are so close together that one is always near.
fn whole_bar_tempo(bpm: f32, seconds: f32) -> Option<f32> {
    if !(bpm > 0.0 && seconds > 0.0) {
        return None;
    }
    let beats = seconds * bpm / 60.0;
    if beats > TEMPO_LOOP_MAX_BEATS {
        return None;
    }
    let bars = (beats / 4.0).round().max(1.0);
    let fitted = bars * 4.0 * 60.0 / seconds;
    ((fitted - bpm).abs() <= TEMPO_LOOP_SNAP_BPM).then_some(fitted)
}

/// Offline tempo detector for the Inspector's Detect and Auto Find. Callers
/// run it on a background worker after decoding the clip's window to mono; it
/// is not a realtime DSP function.
///
/// The measuring is [`SphereAudioProcessor::estimate_bpm_candidates`], the
/// same analysis as the Find Tempo & Key window, so the two never disagree.
/// What this adds is the choice a fit needs:
///
/// * **The octave nearest the project tempo.** Half and double time are the
///   same pulse counted at another level; the one closest to the project
///   needs the least stretch, so it is the one the clip is fitted at. ÷2 and
///   ×2 in the Inspector correct it.
/// * **Whole bars for a loop** (see [`whole_bar_tempo`]).
///
/// Tempos that are not half or double the measured one are only ever offered
/// when the audio itself supports them. Deriving 3:2 or 4:3 readings from the
/// winner and scoring them as if they had been measured is what read loops at
/// tempos like 124.88 against a 180 BPM project.
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
    let measured =
        SphereAudioProcessor::estimate_bpm_candidates(samples, sample_rate, min_bpm, max_bpm);
    let best = *measured.first()?;
    let project = project_bpm.filter(|bpm| bpm.is_finite() && *bpm > 0.0);
    let stretch_from_project = |bpm: f32| project.map_or(0.0, |p| (bpm / p).log2().abs());
    let (octave_bpm, relation) = [
        (1.0_f32, TempoRelation::Raw),
        (0.5, TempoRelation::Half),
        (2.0, TempoRelation::Double),
    ]
    .into_iter()
    .map(|(ratio, relation)| (best.bpm * ratio, relation))
    .filter(|(bpm, _)| *bpm >= min_bpm && *bpm <= max_bpm)
    .min_by(|a, b| stretch_from_project(a.0).total_cmp(&stretch_from_project(b.0)))
    .unwrap_or((best.bpm, TempoRelation::Raw));

    let seconds = samples.len() as f32 / sample_rate;
    let bar_fit = whole_bar_tempo(octave_bpm, seconds);
    let bpm = bar_fit.unwrap_or(octave_bpm);

    let mut candidates: Vec<TempoCandidate> = measured
        .iter()
        .map(|c| TempoCandidate {
            bpm: c.bpm,
            confidence: c.confidence,
            relation: TempoRelation::Raw,
        })
        .collect();
    if candidates.iter().all(|c| (c.bpm - bpm).abs() >= 0.05) {
        candidates.insert(
            0,
            TempoCandidate {
                bpm,
                confidence: best.confidence,
                relation,
            },
        );
    }
    let mut alternatives: Vec<f32> = candidates.iter().map(|c| c.bpm).collect();
    alternatives.sort_by(f32::total_cmp);
    alternatives.dedup_by(|a, b| (*a - *b).abs() < 0.05);

    let confidence = best.confidence.clamp(0.0, 1.0);
    let selection_reason = match (relation, bar_fit.is_some()) {
        (TempoRelation::Raw, false) => "strongest pulse".to_string(),
        (TempoRelation::Raw, true) => "strongest pulse, whole bars".to_string(),
        (_, false) => "octave nearest the project tempo".to_string(),
        (_, true) => "octave nearest the project tempo, whole bars".to_string(),
    };
    Some(TempoDetectionResult {
        bpm,
        confidence,
        low_confidence: confidence < TEMPO_LOW_CONFIDENCE,
        alternatives,
        candidates,
        selection_reason,
    })
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

    /// Repair a Warp clip that never made a pitch choice. Every way into Warp
    /// sets one (`algorithm` becomes Phase Vocoder or Resample Only), except
    /// the Audio Editor's first marker, which used to set the mode alone —
    /// from an unstretched clip that left `Auto` with pitch not kept, so the
    /// clip re-pitched with every marker. It keeps its pitch, as it would
    /// have from the Inspector. For loading projects saved with that.
    pub fn repair_undecided_warp_pitch(&mut self) {
        if self.mode == StretchMode::Warp
            && self.algorithm == StretchAlgorithm::Auto
            && !self.preserve_pitch
        {
            self.apply_keep_pitch(true);
        }
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
    ///
    /// A Tempo Sync clip with no tempo of its own has nothing to lock to: the
    /// engine plays it one to one in seconds, so it is measured in seconds
    /// here too, or the clip would be drawn one length and heard another.
    pub fn follows_project_tempo(&self) -> bool {
        match self.mode {
            StretchMode::Warp => true,
            StretchMode::TempoSync => self.valid_source_bpm().is_some(),
            _ => false,
        }
    }

    fn valid_source_bpm(&self) -> Option<f64> {
        self.bpm_source.filter(|bpm| bpm.is_finite() && *bpm > 0.0)
    }

    /// Seconds of source audio in the clip's window, or `None` while the
    /// window has not been decoded.
    pub fn source_window_seconds(&self) -> Option<f64> {
        let rate = self.source_sample_rate();
        let len = self.source_len_samples();
        (rate > 0 && len > 0).then(|| len as f64 / rate as f64)
    }

    /// The tempo the clip's audio must be at for its window to span `beats`
    /// once it is fitted to the project tempo: `beats × 60 / window seconds`.
    ///
    /// A clip fitted to the tempo plays one of its own beats per project beat
    /// wherever the project tempo goes, so its length in beats is
    /// `window seconds × source BPM / 60` under any tempo map — this is that
    /// equation solved for the source BPM. Kept to the stretch the engine
    /// accepts at `project_bpm`. `None` while the window is undecoded.
    pub fn source_bpm_for_beats(&self, beats: f64, project_bpm: f64) -> Option<f64> {
        let seconds = self.source_window_seconds()?;
        if !(beats.is_finite() && beats > 0.0 && project_bpm.is_finite() && project_bpm > 0.0) {
            return None;
        }
        let bpm = beats * 60.0 / seconds;
        Some(bpm.clamp(project_bpm * Self::MIN_RATIO, project_bpm * Self::MAX_RATIO))
    }

    /// Length in beats of the clip once fitted to the tempo, from its own
    /// tempo: `window seconds × source BPM / 60`, under any tempo map.
    pub fn fitted_beats(&self) -> Option<f64> {
        Some(self.source_window_seconds()? * self.valid_source_bpm()? / 60.0)
    }

    /// Next state fitted to the project tempo with `source_bpm` as the clip's
    /// own tempo: Tempo timing, keeping the clip's pitch choice, so the clip
    /// follows every tempo change from now on.
    ///
    /// A Warp clip is already locked to the tempo by its markers; it only
    /// records the tempo, and keeps its markers.
    pub fn fitted_to_source_bpm(&self, source_bpm: f64, project_bpm: f64) -> Self {
        let mut next = match self.mode {
            StretchMode::Warp | StretchMode::TempoSync => self.clone(),
            _ => self.with_timing(StretchTiming::Tempo, project_bpm),
        };
        if !(source_bpm.is_finite() && source_bpm > 0.0) {
            return next;
        }
        let project_bpm = if project_bpm.is_finite() && project_bpm > 0.0 {
            project_bpm
        } else {
            source_bpm
        };
        let source_bpm =
            source_bpm.clamp(project_bpm * Self::MIN_RATIO, project_bpm * Self::MAX_RATIO);
        next.bpm_source = Some(source_bpm);
        if next.mode == StretchMode::TempoSync {
            next.bpm_target = Some(project_bpm);
            next.set_stretch_ratio(Self::source_bpm_to_project_bpm_ratio(
                source_bpm,
                project_bpm,
            ));
        }
        next.clip_timeline_duration_beats = 0.0;
        next.dirty = true;
        next
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

    /// The Audio Editor's first marker used to set the Warp mode alone, so
    /// the clip re-pitched with every marker. Loading repairs that; a warp
    /// the user set to re-pitch stays as it is.
    #[test]
    fn a_warp_without_a_pitch_choice_keeps_pitch_on_load() {
        let mut undecided = decoded(48_000, 48_000);
        undecided.mode = StretchMode::Warp;
        assert!(!undecided.keeps_pitch());
        undecided.repair_undecided_warp_pitch();
        assert!(undecided.keeps_pitch());
        assert!(undecided.to_sphere_stretch_params(120.0).preserve_pitch);

        let mut tape = decoded(48_000, 48_000)
            .with_timing(StretchTiming::Warp, 120.0)
            .with_keep_pitch(false);
        tape.repair_undecided_warp_pitch();
        assert!(!tape.keeps_pitch());
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
    fn the_source_tempo_for_a_length_is_the_beats_over_the_window() {
        let mut s = manual_clip(192_000);
        s.original_sample_rate = 48_000;
        s.project_sample_rate = 48_000;
        // 4 s over 8 beats: 120 BPM audio.
        approx(s.source_bpm_for_beats(8.0, 100.0).unwrap(), 120.0);
        // Held to the stretch the engine accepts.
        approx(
            s.source_bpm_for_beats(10_000.0, 100.0).unwrap(),
            100.0 * AudioClipStretchState::MAX_RATIO,
        );
        let fitted = s.fitted_to_source_bpm(120.0, 100.0);
        assert_eq!(fitted.timing(), StretchTiming::Tempo);
        approx(fitted.fitted_beats().unwrap(), 8.0);
        approx(fitted.effective_time_ratio(100.0), 1.2);
    }

    #[test]
    fn a_warp_clip_only_records_a_fitted_tempo() {
        let mut s = manual_clip(192_000);
        s.mode = StretchMode::Warp;
        let fitted = s.fitted_to_source_bpm(120.0, 100.0);
        assert_eq!(fitted.mode, StretchMode::Warp);
        assert_eq!(fitted.bpm_source, Some(120.0));
        approx(fitted.stretch_ratio, s.stretch_ratio);
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

    /// A drum loop: kick on every beat, snare on 2 and 4, closed hats on
    /// eighths, over a sustained bass note.
    fn drum_loop(bpm: f32, seconds: f32, sample_rate: f32) -> Vec<f32> {
        let n = (sample_rate * seconds) as usize;
        let beat = 60.0 / bpm;
        let mut state = 7u32;
        let mut noise = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate;
                let in_beat = t % beat;
                let beat_index = (t / beat) as usize;
                let in_eighth = t % (beat * 0.5);
                let kick =
                    (-in_beat * 30.0).exp() * (2.0 * std::f32::consts::PI * 55.0 * in_beat).sin();
                let snare = if beat_index % 2 == 1 {
                    (-in_beat * 25.0).exp() * noise() * 0.6
                } else {
                    0.0
                };
                let hat = (-in_eighth * 120.0).exp() * noise() * 0.25;
                let bass = 0.5 * (2.0 * std::f32::consts::PI * 41.2 * t).sin();
                kick + snare + hat + bass
            })
            .collect()
    }

    const DETECT_RATE: f32 = 22_050.0;

    #[test]
    fn detect_tempo_from_mono_finds_simple_pulse() {
        let samples = drum_loop(120.0, 20.0, DETECT_RATE);
        let result =
            detect_tempo_from_mono(&samples, DETECT_RATE, 60.0, 200.0, Some(120.0)).unwrap();
        assert!((result.bpm - 120.0).abs() <= 0.1, "{result:?}");
        assert!(!result.low_confidence, "{result:?}");
    }

    /// The project tempo picks the octave, never a tempo the audio is not at:
    /// a 118 BPM loop in a 120 BPM project is 118.
    #[test]
    fn a_tempo_near_the_project_is_not_pulled_onto_it() {
        let samples = drum_loop(118.0, 20.0, DETECT_RATE);
        let result =
            detect_tempo_from_mono(&samples, DETECT_RATE, 60.0, 200.0, Some(120.0)).unwrap();
        assert!((result.bpm - 118.0).abs() <= 0.1, "{result:?}");
    }

    /// No 3:2 or 3:4 reading of the pulse is invented: a loop at 166 BPM in a
    /// 180 BPM project reads 166 (once read as 124.88 = 166.5 × 3/4).
    #[test]
    fn related_tempos_are_measured_not_derived() {
        let samples = drum_loop(166.0, 20.0, DETECT_RATE);
        let result =
            detect_tempo_from_mono(&samples, DETECT_RATE, 60.0, 200.0, Some(180.0)).unwrap();
        assert!((result.bpm - 166.0).abs() <= 0.15, "{result:?}");
    }

    /// Half and double time are the same pulse; the clip is fitted at the one
    /// nearest the project, which needs the least stretch.
    #[test]
    fn the_octave_nearest_the_project_is_chosen() {
        let samples = drum_loop(87.0, 20.0, DETECT_RATE);
        let fast =
            detect_tempo_from_mono(&samples, DETECT_RATE, 60.0, 200.0, Some(170.0)).unwrap();
        assert!((fast.bpm - 174.0).abs() <= 0.2, "{fast:?}");
        let slow =
            detect_tempo_from_mono(&samples, DETECT_RATE, 60.0, 200.0, Some(90.0)).unwrap();
        assert!((slow.bpm - 87.0).abs() <= 0.1, "{slow:?}");
    }

    /// A loop cut on its bar lines is read at the tempo that makes it whole
    /// bars, finer than onsets can measure.
    #[test]
    fn a_loop_is_read_at_whole_bars() {
        let bpm = 123.37_f32;
        let seconds = 32.0 * 60.0 / bpm;
        let samples = drum_loop(bpm, seconds, DETECT_RATE);
        let result =
            detect_tempo_from_mono(&samples, DETECT_RATE, 60.0, 200.0, Some(120.0)).unwrap();
        let beats = samples.len() as f32 / DETECT_RATE * result.bpm / 60.0;
        assert!((beats - 32.0).abs() < 1.0e-3, "{beats} beats: {result:?}");
    }

    #[test]
    fn whole_bars_never_move_a_tempo_far() {
        // 10 s at 120 BPM is 20 beats = 5 bars exactly.
        assert_eq!(whole_bar_tempo(120.1, 10.0), Some(120.0));
        // 4.5 bars: no whole bar count is near.
        assert_eq!(whole_bar_tempo(108.0, 10.0), None);
        // A whole song is not a loop.
        assert_eq!(whole_bar_tempo(120.1, 200.0), None);
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

    #[test]
    fn manual_source_bpm_drives_fit_project_ratio() {
        // Fix 10 case 4: user types 118, project 120 → ratio 118/120 = 0.98333.
        let mut s = manual_clip(48_000);
        s.bpm_source = Some(118.0);
        assert!(s.fit_to_project_tempo(120.0));
        approx(s.stretch_ratio, 118.0 / 120.0);
        approx(s.stretch_ratio, 0.983_333_333_333_333_3);
    }
}

//! Runtime clips, fades, and the clip DSP and stretch processors.

use super::*;

/// Shape of one clip fade ramp.
///
/// The two laws answer different questions, which is why this is a choice and
/// not a constant:
///
/// * **Equal power** holds the *power* sum of a crossfade constant. Two
///   unrelated takes fading into each other are uncorrelated, so their powers
///   add — a linear pair would dip about 3 dB in the middle, heard as a hole.
/// * **Linear** holds the *amplitude* sum constant. Two halves of the same
///   continuous performance are perfectly correlated, so their amplitudes add —
///   an equal-power pair would bulge about 3 dB in the middle instead.
///
/// Equal power is the default because an overlap between two different clips is
/// the common case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FadeCurve {
    #[default]
    EqualPower,
    Linear,
}

impl FadeCurve {
    /// Parse the tag the project and the engine snapshot carry.
    ///
    /// Anything unrecognised is equal power rather than an error: the tag
    /// crosses a serialization boundary, and a project written by a build that
    /// knows a curve this one does not should still play.
    pub fn from_tag(tag: &str) -> Self {
        match tag.trim().to_ascii_lowercase().as_str() {
            "linear" => Self::Linear,
            _ => Self::EqualPower,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::EqualPower => "equal_power",
            Self::Linear => "linear",
        }
    }

    /// Gain at `t` in `0..=1` along a rising ramp. The falling ramp is the same
    /// law read backwards, which is what makes a pair of them sum correctly.
    #[inline]
    pub fn rising_gain(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::EqualPower => (t * std::f32::consts::FRAC_PI_2).sin(),
            Self::Linear => t,
        }
    }

    #[inline]
    pub fn falling_gain(self, t: f32) -> f32 {
        self.rising_gain(1.0 - t.clamp(0.0, 1.0))
    }
}

pub struct RuntimeClip {
    pub id: String,
    pub track_id: String,
    /// [`Self::track_id`] resolved to a track index at build time
    /// ([`RuntimeProject::resolve_indices`]); `None` when the track is missing.
    pub track_index: Option<usize>,
    /// Canonical musical start used to rebuild sample positions when the device
    /// sample rate changes.
    pub start_beat: f64,
    /// Canonical musical duration used to rebuild sample positions when the
    /// device sample rate changes.
    pub duration_beats: f64,
    pub start_sample: u64,
    pub duration_samples: u64,
    pub offset_seconds: f64,
    /// The clip gain being played now, moving toward [`Self::gain_target`]
    /// a sample at a time (see [`clip_gain_step`]): a live change, dragged
    /// in the arrangement, ramps instead of stepping.
    pub gain: f32,
    /// The clip gain set (linear, 0..4). Live-updated by `SetClipGain`.
    pub gain_target: f32,
    /// How far `gain` moves toward the target each sample.
    pub gain_step: f32,
    /// Immutable stretch parameters copied from the project snapshot for the
    /// audio thread. SphereAudioProcessor is the source of truth for all derived
    /// ratios/backend decisions.
    pub stretch: StretchParams,
    pub speed_ratio: f32,
    pub source_read_rate: f32,
    pub effective_time_ratio: f32,
    pub pitch_ratio: f32,
    pub stretch_backend: StretchBackend,
    pub source_start_samples: u64,
    pub source_end_samples: u64,
    pub warp_markers: Vec<RuntimeWarpMarker>,
    /// Precomputed output→source segments. Empty means the clip uses the
    /// global `source_read_rate`. Built on the control thread.
    pub warp_segments: Vec<RuntimeWarpSegment>,
    pub processor: ClipDspProcessor,
    /// Play the source window backwards (resolved from the snapshot's
    /// `audio_process.reverse`). The render maps output → source from the clip
    /// end instead of the start; `speed_ratio` is unchanged.
    pub reverse: bool,
    /// Per-clip adaptive de-noise state. It is prepared with the runtime graph
    /// and only mutated by the render thread, so the callback never allocates.
    pub denoise: DenoiseProcessor,
    pub id_hash: u64,
    pub channel_transform: SphereAudioProcessor::ChannelTransform,
    pub dc_remove: bool,
    pub dc_left: f32,
    pub dc_right: f32,
    pub extra_gain: f32,
    pub dehum: SphereAudioProcessor::DehumProcessor,
    pub envelope_points: Vec<(f32, f32)>,
    pub preview_bypass: bool,
    /// Clip-level mute — a muted clip is skipped entirely during render.
    pub muted: bool,
    /// An ARA plug-in owns this clip's playback.
    ///
    /// The clip loop skips it: the plug-in already read the source samples out
    /// of band through the ARA host callbacks and renders the result itself, so
    /// mixing the file in here as well would double the audio.
    pub ara_rendered: bool,
    /// Equal-power fade lengths in output samples, resolved from the snapshot's
    /// fade durations at build time. `0` means no fade. Clamped so
    /// `fade_in + fade_out <= duration_samples` (see `clip_fade_gain`).
    pub fade_in_samples: u64,
    pub fade_out_samples: u64,
    /// Shape of each ramp. Carried per edge because the two ends of a clip are
    /// usually different jobs: the tail of one take crossfading into the head
    /// of another wants equal power, while a fade against silence — or a cut
    /// inside one continuous performance — wants linear.
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
    pub source: Arc<ClipAudioSource>,
    /// Cached preserve-pitch processor for this runtime clip/voice. Created on
    /// the control thread while building/cloning the runtime graph; the audio
    /// thread only calls `reset`/`process_stereo` on it.
    pub stretch_processor: Option<Box<dyn StretchProcessor + Send>>,
    pub stretch_input_l: Vec<f32>,
    pub stretch_input_r: Vec<f32>,
    pub stretch_output_l: Vec<f32>,
    pub stretch_output_r: Vec<f32>,
    /// Pre-roll scratch fed to `StretchProcessor::output_seek` to latency-align
    /// the stretcher output to the timeline on (re)start. Grows lazily to the
    /// largest seek length seen, then stays stable (no steady-state alloc).
    pub stretch_prime_l: Vec<f32>,
    pub stretch_prime_r: Vec<f32>,
    pub stretch_next_project_sample: Option<u64>,
}

impl std::fmt::Debug for RuntimeClip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeClip")
            .field("id", &self.id)
            .field("track_id", &self.track_id)
            .field("track_index", &self.track_index)
            .field("start_beat", &self.start_beat)
            .field("duration_beats", &self.duration_beats)
            .field("start_sample", &self.start_sample)
            .field("duration_samples", &self.duration_samples)
            .field("offset_seconds", &self.offset_seconds)
            .field("gain", &self.gain)
            .field("stretch", &self.stretch)
            .field("source_read_rate", &self.source_read_rate)
            .field("effective_time_ratio", &self.effective_time_ratio)
            .field("pitch_ratio", &self.pitch_ratio)
            .field("stretch_backend", &self.stretch_backend)
            .field("processor", &self.processor)
            .field("reverse", &self.reverse)
            .field("muted", &self.muted)
            .field("has_stretch_processor", &self.stretch_processor.is_some())
            .finish()
    }
}

impl Clone for RuntimeClip {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            track_id: self.track_id.clone(),
            track_index: self.track_index,
            start_beat: self.start_beat,
            duration_beats: self.duration_beats,
            start_sample: self.start_sample,
            duration_samples: self.duration_samples,
            offset_seconds: self.offset_seconds,
            gain: self.gain,
            gain_target: self.gain_target,
            gain_step: self.gain_step,
            stretch: self.stretch.clone(),
            speed_ratio: self.speed_ratio,
            source_read_rate: self.source_read_rate,
            effective_time_ratio: self.effective_time_ratio,
            pitch_ratio: self.pitch_ratio,
            stretch_backend: self.stretch_backend,
            source_start_samples: self.source_start_samples,
            source_end_samples: self.source_end_samples,
            warp_markers: self.warp_markers.clone(),
            warp_segments: self.warp_segments.clone(),
            processor: self.processor,
            reverse: self.reverse,
            denoise: DenoiseProcessor::new(self.denoise.sample_rate(), self.denoise.amount()),
            id_hash: self.id_hash,
            channel_transform: self.channel_transform,
            dc_remove: self.dc_remove,
            dc_left: self.dc_left,
            dc_right: self.dc_right,
            extra_gain: self.extra_gain,
            dehum: SphereAudioProcessor::DehumProcessor::new(
                self.denoise.sample_rate(),
                self.dehum.params(),
            ),
            envelope_points: self.envelope_points.clone(),
            preview_bypass: self.preview_bypass,
            muted: self.muted,
            ara_rendered: self.ara_rendered,
            fade_in_samples: self.fade_in_samples,
            fade_out_samples: self.fade_out_samples,
            fade_in_curve: self.fade_in_curve,
            fade_out_curve: self.fade_out_curve,
            source: Arc::clone(&self.source),
            stretch_processor: create_runtime_stretch_processor(
                self.stretch_backend,
                self.source.sample_rate(),
                &self.stretch,
            ),
            // Keep the capacity the build sized for this clip (a warped
            // clip's fastest segment can need more than its global ratio).
            stretch_input_l: vec![0.0; self.stretch_input_l.len()],
            stretch_input_r: vec![0.0; self.stretch_input_r.len()],
            stretch_output_l: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
            stretch_output_r: vec![0.0; DEFAULT_AUDIO_BLOCK_CAPACITY],
            stretch_prime_l: vec![0.0; self.stretch_prime_l.len()],
            stretch_prime_r: vec![0.0; self.stretch_prime_r.len()],
            stretch_next_project_sample: None,
        }
    }
}

/// Capacity required for the largest source span a stretcher can consume for
/// one engine block. The reciprocal is intentionally bounded by the same
/// policy as SphereAudioProcessor, so an invalid project value cannot trigger
/// an unbounded allocation here.
pub(super) fn stretch_input_capacity(time_ratio: f32) -> usize {
    let ratio = if time_ratio.is_finite() && time_ratio > 0.0 {
        time_ratio.clamp(0.05, 20.0) as f64
    } else {
        1.0
    };
    let source_frames = (DEFAULT_AUDIO_BLOCK_CAPACITY as f64 / ratio).ceil();
    source_frames
        .min((DEFAULT_AUDIO_BLOCK_CAPACITY * 20) as f64)
        .max(1.0) as usize
        + 2
}

pub type AudioClip = EngineClipSnapshot;

/// `FUTUREBOARD_CLIP_DSP_DEBUG=1` enables a one-line-per-clip diagnostic of the
/// resolved stretch DSP path, printed once at graph-build time (never from the
/// audio callback). Cached on first read.
pub fn clip_dsp_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_CLIP_DSP_DEBUG").is_some())
}

/// The clip-stretch DSP path resolved for a clip. `PhaseVocoderBasic` is a
/// basic streaming OLA/granular stretcher today; the enum keeps processor
/// selection explicit and leaves room for a higher-quality phase vocoder without
/// changing snapshot/runtime wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipDspProcessor {
    NoStretch,
    Resample,
    PhaseVocoderBasic,
}

/// Resolve the DSP path from the snapshot's `mode` key (see
/// `engine_snapshot::stretch_mode_key`) and `preserve_pitch` flag.
#[cfg(test)]
pub fn resolve_clip_processor(mode: &str, preserve_pitch: bool) -> ClipDspProcessor {
    match mode {
        "off" | "none" => ClipDspProcessor::NoStretch,
        "resample" => ClipDspProcessor::Resample,
        "manual" | "temposync" => {
            if preserve_pitch {
                ClipDspProcessor::PhaseVocoderBasic
            } else {
                ClipDspProcessor::Resample
            }
        }
        "warp" => ClipDspProcessor::PhaseVocoderBasic,
        _ => ClipDspProcessor::Resample,
    }
}

pub(super) fn resolved_clip_stretch_params(clip: &EngineClipSnapshot) -> StretchParams {
    let params = if clip.stretch != StretchParams::default() {
        clip.stretch.clone()
    } else {
        clip.audio_process
            .as_ref()
            .map(legacy_process_stretch_params)
            .unwrap_or_default()
    };
    params.sanitized()
}

pub(super) fn legacy_process_stretch_params(process: &EngineClipAudioProcess) -> StretchParams {
    legacy_audio_process_to_stretch(
        &process.mode,
        process.preserve_pitch,
        process.speed_ratio,
        process.effective_time_ratio,
        process.pitch_semitones,
        &process.quality,
    )
}

pub(super) fn legacy_audio_process_to_stretch(
    mode: &str,
    preserve_pitch: bool,
    speed_ratio: f64,
    effective_time_ratio: f64,
    pitch_semitones: f64,
    quality: &str,
) -> StretchParams {
    let mut params = StretchParams::default();
    let legacy_time_ratio = if effective_time_ratio.is_finite() && effective_time_ratio > 0.0 {
        effective_time_ratio as f32
    } else if speed_ratio.is_finite() && speed_ratio > 0.0 {
        (1.0 / speed_ratio) as f32
    } else {
        1.0
    };

    let mode_key = mode.to_ascii_lowercase();
    let force_repitch = mode_key == "resample";
    params.mode = match mode_key.as_str() {
        "off" | "none" => StretchMode::Off,
        "temposync" | "tempo_sync" | "tempo-sync" => StretchMode::TempoSync,
        "warp" => StretchMode::Warp,
        "manual" | "resample" => {
            if (legacy_time_ratio - 1.0).abs() > f32::EPSILON || preserve_pitch || force_repitch {
                StretchMode::Manual
            } else {
                StretchMode::Off
            }
        }
        _ => {
            if (legacy_time_ratio - 1.0).abs() > f32::EPSILON || preserve_pitch {
                StretchMode::Manual
            } else {
                StretchMode::Off
            }
        }
    };
    params.algorithm = if params.mode == StretchMode::Off {
        StretchAlgorithm::Off
    } else if preserve_pitch && !force_repitch {
        StretchAlgorithm::PreservePitch
    } else {
        StretchAlgorithm::RePitch
    };
    params.time_ratio = legacy_time_ratio;
    params.pitch_ratio = if pitch_semitones.is_finite() {
        2.0_f32.powf(pitch_semitones as f32 / 12.0)
    } else {
        1.0
    };
    params.preserve_pitch = preserve_pitch && !force_repitch && params.mode != StretchMode::Off;
    params.quality = match quality {
        "draft" => 0.35,
        "high" => 1.0,
        _ => 0.75,
    };
    params
}

pub fn resolve_clip_processor_from_stretch(params: &StretchParams) -> ClipDspProcessor {
    if params.mode == StretchMode::Off || params.algorithm == StretchAlgorithm::Off {
        return ClipDspProcessor::NoStretch;
    }
    match resolve_backend(params) {
        StretchBackend::InternalRePitch => ClipDspProcessor::Resample,
        StretchBackend::Signalsmith => ClipDspProcessor::PhaseVocoderBasic,
    }
}

pub fn describe_clip_dsp_state(
    clip: &AudioClip,
    process: &EngineClipAudioProcess,
    project_bpm: f64,
) -> String {
    let stretch = legacy_process_stretch_params(process);
    let processor = resolve_clip_processor_from_stretch(&stretch);
    let pending = if matches!(processor, ClipDspProcessor::PhaseVocoderBasic)
        && process.pitch_semitones.abs() > f64::EPSILON
    {
        " pitch_shift=pending"
    } else {
        ""
    };
    let duration_samples = process
        .source_end_samples
        .saturating_sub(process.source_start_samples) as f64
        * process.effective_time_ratio.max(0.0);
    format!(
        "Clip DSP Snapshot: clip_id={} name={} mode={} ratio={:.6} percent={:.2} algorithm={} effective_time_ratio={:.6} pitch_ratio={:.6} speed_ratio={:.6} preserve_pitch={} reverse={} duration_samples={} source_start={} source_end={} processor={:?}{} warp_markers={} project_bpm={:.3}",
        clip.id,
        clip.asset_id,
        process.mode,
        effective_time_ratio(&stretch, Some(project_bpm as f32)),
        effective_time_ratio(&stretch, Some(project_bpm as f32)) * 100.0,
        process.quality,
        effective_time_ratio(&stretch, Some(project_bpm as f32)),
        effective_pitch_ratio(&stretch),
        source_read_rate_for_repitch(&stretch, Some(project_bpm as f32)),
        process.preserve_pitch,
        process.reverse,
        duration_samples.round() as u64,
        process.source_start_samples,
        process.source_end_samples,
        processor,
        pending,
        process.warp_markers.len(),
        project_bpm,
    )
}

/// Switch for routing preserve-pitch clips through the real Signalsmith backend
/// in the realtime render. **Default-on**: the Signalsmith default preset reports
/// ~5760 samples (≈120 ms @ 48 kHz) of algorithmic latency, which is now
/// compensated per-clip via `output_seek` pre-roll priming in
/// `render_signalsmith_clip_segment` (the next `process` output is aligned to the
/// playback position on every (re)start), so stretched clips stay in sync without
/// the crude zero-latency `PhaseVocoderBasic` fallback. Set
/// `FUTUREBOARD_STRETCH_SIGNALSMITH=0` to force the fallback for A/B comparison.
pub(super) fn signalsmith_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| match std::env::var("FUTUREBOARD_STRETCH_SIGNALSMITH") {
        Ok(value) => {
            let v = value.trim();
            !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
        }
        Err(_) => true,
    })
}

/// Build the per-clip preserve-pitch stretch processor for the realtime render.
///
/// Only the Signalsmith backend uses a cached `StretchProcessor`; resample /
/// no-stretch clips are sampled inline. The bridge is now an allocation-free
/// pass-through (`render_signalsmith_clip_segment` feeds it exactly the source
/// samples it consumes per block), so it is safe for the audio callback. Created
/// on the control thread; the audio thread only calls `reset`/`process_stereo`.
pub(super) fn create_runtime_stretch_processor(
    backend: StretchBackend,
    sample_rate: u32,
    stretch: &StretchParams,
) -> Option<Box<dyn StretchProcessor + Send>> {
    if backend != StretchBackend::Signalsmith || !signalsmith_enabled() {
        return None;
    }
    match create_stretch_processor(backend, sample_rate as f32, 2, stretch.clone()) {
        Ok(processor) => {
            if std::env::var_os("FUTUREBOARD_AUDIO_DEBUG").is_some() {
                eprintln!(
                    "[clip-stretch] signalsmith processor created sample_rate={sample_rate} latency_samples={} time_ratio={:.4} pitch_ratio={:.4}",
                    processor.latency_samples(),
                    effective_time_ratio(stretch, None),
                    effective_pitch_ratio(stretch),
                );
            }
            Some(processor)
        }
        Err(err) => {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[clip-stretch] signalsmith processor unavailable, using fallback: {err}"
                );
            }
            None
        }
    }
}

/// `tempo_map` is `Some` only when the project's tempo is not one flat value
/// (see [`tempo_map_is_flat_at`]); a flat map keeps the base-tempo arithmetic
/// below exactly as it was.
/// How long a live clip gain change takes to arrive.
pub(super) const CLIP_GAIN_RAMP_SECONDS: f64 = 0.008;

/// The per-sample step of a clip's gain toward its target at `sample_rate`:
/// a one-pole that covers [`CLIP_GAIN_RAMP_SECONDS`].
pub(crate) fn clip_gain_step(sample_rate: u32) -> f32 {
    let samples = CLIP_GAIN_RAMP_SECONDS * f64::from(sample_rate.max(1));
    (1.0 - (-1.0 / samples).exp()) as f32
}

//! Central audio-processing API surface for Futureboard Studio.
//!
//! This crate owns shared, serializable stretch parameters and pure ratio /
//! backend-selection math so UI, playback, export, waveform cache, and timeline
//! length can converge on one source of truth.
//!
//! It also re-exports the offline MDX-NET stem-extraction params/model/device
//! surface from `SphereStemExtractor` for the Stem Extractor dialog and jobs.

pub mod analysis;
pub mod clip_process;
pub mod denoise;
pub mod ffi;
pub mod stem;
pub mod stretching;

pub use analysis::{
    AnalysisOptions, AudioAnalysis, Classifier, FftSize, FrequencyFocus, HeuristicClassifier,
    InstrumentCategory, InstrumentEstimate, KeyEstimate, KeyMode, LoudnessMeasurement,
    PhaseMeasurement, PitchClass, SpectralFeatures, SpectrumMode, SpectrumSmoothing,
    SpectrumSnapshot, SpectrumWindow, TempoCandidate, TempoEstimate, TempoFamily, TempoHypothesis,
    TransientDetectParams, TransientMarker, analyze_loudness, analyze_mono, analyze_mono_with,
    analyze_ring_window, analyze_spectrum, analyze_stereo, detect_transients,
    estimate_bpm_candidates, estimate_key_ranked, measure_phase, octave_ratio, pitch_class_profile,
    rank_keys, tempo_families,
};

pub use clip_process::{
    AudioClipProcessor, ChannelTransform, ClickEvent, DcOffset, DcOffsetProcessor, DeclickParams,
    DehumParams, DehumProcessor, NoiseGateMask, NormalizeMeasurement, NormalizeMode,
    NormalizeParams, ResampleError, SpectralDenoiseParams, SpectralGainParams, StftSettings,
    apply_channel_transform, apply_channel_transform_interleaved, apply_gain_interleaved,
    apply_spectral_gain, db_to_lin, declick_interleaved, detect_clicks, downmix_interleaved,
    interpolate_spectral_region, learn_noise_profile, lin_to_db, measure_dc_offset,
    measure_normalize, noise_gate_mask, peak_amplitude, reduce_noise_stft, replace_frame_range,
    required_normalize_gain_db, resample_interleaved, slice_frames, write_wav_f32,
};

pub use denoise::DenoiseProcessor;

pub use stem::{
    InferBackendKind, InferDevice, STEM_MODELS, StemExtractCancelToken, StemExtractError,
    StemExtractInput, StemExtractOutput, StemExtractParams, StemExtractProgress,
    StemExtractQuality, StemExtractResult, StemExtractStage, StemInferBackend, StemKind, StemModel,
    StemModelDownloadProgress, StemModelFile, StemModelInfo, StemModelPackage, StemPlatform,
    StemPlatformRuntime, StemSet, UVR_MODEL_RELEASE_BASE, auto_stem_extract_params,
    create_mdx_net_backend, current_stem_platform, default_models_dir, default_stem_extract_params,
    download_model, ensure_models_dir, extract_stems, gpu_available, mdx_net_gpu_params,
    model_installed, resolve_current_platform_runtime, resolve_device, resolve_installed_model_files,
    resolve_platform_runtime, set_gpu_detected,
};
pub use stretching::{
    StretchAlgorithm, StretchBackend, StretchError, StretchMode, StretchParams, StretchProcessor,
    create_stretch_processor, effective_pitch_ratio, effective_time_ratio,
    pitch_ratio_to_semitone_cents, render_stretch_interleaved, resolve_backend,
    semitone_to_pitch_ratio, signalsmith_stretch_available, source_read_rate_for_repitch,
    stretched_duration_samples,
};

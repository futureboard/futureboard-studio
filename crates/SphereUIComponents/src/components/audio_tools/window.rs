//! Independent GPUI windows for audio editor analysis and processing tools.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement,
    MouseMoveEvent, ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};
use sphere_audio_editor::{
    AudioRangeSelection, AudioRepairModule, AudioToolKind, AudioToolSession, AudioToolTarget,
    SpectralSelection,
};
use SphereAudioProcessor::{
    analyze_loudness, analyze_spectrum, apply_channel_transform_interleaved,
    apply_gain_interleaved, apply_spectral_gain, db_to_lin, declick_interleaved, detect_transients,
    downmix_interleaved, estimate_bpm_candidates, estimate_key_ranked, learn_noise_profile,
    measure_dc_offset, measure_normalize, measure_phase, reduce_noise_stft,
    render_stretch_interleaved, replace_frame_range, resample_interleaved, semitone_to_pitch_ratio,
    slice_frames, write_wav_f32, AudioClipProcessor, ChannelTransform, DcOffsetProcessor,
    DeclickParams, DehumParams, DehumProcessor, FftSize, FrequencyFocus, KeyEstimate,
    LoudnessMeasurement, NormalizeMeasurement, NormalizeMode, NormalizeParams, PhaseMeasurement,
    SpectralDenoiseParams, SpectralGainParams, SpectrumMode, SpectrumSmoothing, SpectrumSnapshot,
    SpectrumWindow, StftSettings, StretchAlgorithm, StretchMode, StretchParams, TempoCandidate,
    TransientDetectParams, TransientMarker,
};

use crate::components::inspector::inspector_mini_button;
use crate::components::timeline::Timeline;
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{space, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

use super::preview::ClipPreviewOverride;
use super::repair::RepairSurface;
use super::viz::{self, DisplaySmoothing, GraphDraw, GraphStyle};
use super::workspace;

pub const AUDIO_TOOL_WINDOW_MIN_WIDTH: f32 = 480.0;
pub const AUDIO_TOOL_WINDOW_MIN_HEIGHT: f32 = 320.0;

const GONIO_TRAIL: usize = 192;
const LEVEL_HOPS: usize = 256;
const AVAILABLE_REPAIR: [AudioRepairModule; 4] = [
    AudioRepairModule::Denoise,
    AudioRepairModule::DeClick,
    AudioRepairModule::DeHum,
    AudioRepairModule::SpectralRepair,
];

const REFRESH: Duration = Duration::from_millis(50);

pub enum AudioToolCommand {
    Preview(ClipPreviewOverride),
    ClearPreview(String),
    MutateClip {
        clip_id: String,
        label: &'static str,
        mutate: Box<dyn FnOnce(&mut crate::components::timeline::timeline_state::ClipState) + Send>,
    },
    AddMarkers {
        beats: Vec<f64>,
        label: &'static str,
    },
    AddWarpMarkers {
        clip_id: String,
        frames: Vec<u64>,
    },
    SliceClip {
        clip_id: String,
        beats: Vec<f32>,
    },
    UseOriginalBpm {
        clip_id: String,
        bpm: f64,
    },
    AddTempoPoint {
        beat: f64,
        bpm: f64,
    },
    ReplaceSource {
        clip_id: String,
        path: PathBuf,
        sample_rate: u32,
    },
}

#[derive(Clone)]
pub struct AudioToolWindowCallbacks {
    pub on_command: Arc<dyn Fn(AudioToolCommand, &mut App) + Send + Sync>,
    pub on_close: Arc<dyn Fn(AudioToolKind, Bounds<Pixels>, &mut App) + Send + Sync>,
}

pub struct AudioToolWindowManager {
    pub windows: HashMap<AudioToolKind, WindowHandle<AudioToolWindow>>,
    pub last_bounds: HashMap<AudioToolKind, Bounds<Pixels>>,
    pub previews: HashMap<String, ClipPreviewOverride>,
    /// The Find Tempo & Key window, opened from the transport.
    pub tempo_key: Option<WindowHandle<crate::components::tempo_key_finder::TempoKeyFinderWindow>>,
    pub tempo_key_bounds: Option<Bounds<Pixels>>,
}

impl Default for AudioToolWindowManager {
    fn default() -> Self {
        Self {
            windows: HashMap::new(),
            last_bounds: HashMap::new(),
            previews: HashMap::new(),
            tempo_key: None,
            tempo_key_bounds: None,
        }
    }
}

impl AudioToolWindowManager {
    pub fn prune(&mut self, cx: &mut App) {
        self.windows
            .retain(|_, handle| handle.update(cx, |_, _, _| {}).is_ok());
    }

    pub fn follow_selection(&mut self, target: &AudioToolTarget, cx: &mut App) {
        for handle in self.windows.values() {
            let _ = handle.update(cx, |window, _win, cx| {
                if window.session.follow_selection && !window.session.pin_target {
                    window.session.target = target.clone();
                    DirectAudio::analysis_tap().set_target_clip(Some(&target.clip_id));
                    cx.notify();
                }
            });
        }
    }

    pub fn close_all(&mut self, cx: &mut App) {
        for handle in self.windows.values() {
            let _ = handle.update(cx, |_this, window, _cx| {
                window.remove_window();
            });
        }
        self.windows.clear();
        if let Some(handle) = self.tempo_key.take() {
            let _ = handle.update(cx, |_this, window, _cx| window.remove_window());
        }
        self.previews.clear();
        DirectAudio::analysis_tap().set_target_clip(None);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TimePitchMode {
    Off,
    Stretch,
    FitDuration,
    FollowTempo,
}

#[derive(Clone)]
struct AbSnapshot {
    time_mode: TimePitchMode,
    stretch_percent: f64,
    pitch_semi: f32,
    pitch_cents: f32,
    preserve_transients: bool,
    normalize: NormalizeParams,
    channel: ChannelTransform,
    resample_target: u32,
    repair_module: AudioRepairModule,
    denoise: SpectralDenoiseParams,
    declick: DeclickParams,
    dehum: DehumParams,
    spectral_gain_db: f32,
    fft_size: FftSize,
    spectrum_window: SpectrumWindow,
    smoothing: SpectrumSmoothing,
    peak_hold: bool,
    spectrum_mode: SpectrumMode,
    transient_params: TransientDetectParams,
    freq_focus: FrequencyFocus,
    bpm_min: f32,
    bpm_max: f32,
}

pub struct AudioToolWindow {
    pub(crate) session: AudioToolSession,
    pub(super) timeline: Entity<Timeline>,
    callbacks: AudioToolWindowCallbacks,
    pub(super) status: String,
    // Spectrum
    spectrum_mode: SpectrumMode,
    pub(super) fft_size: FftSize,
    spectrum_window: SpectrumWindow,
    smoothing: SpectrumSmoothing,
    peak_hold: bool,
    pub(super) spectrum: Option<SpectrumSnapshot>,
    pub(super) spectrum_avg: Option<Vec<f32>>,
    // Loudness / normalize / dc / bpm / key / transients
    loudness: Option<LoudnessMeasurement>,
    normalize: NormalizeParams,
    measurement: Option<NormalizeMeasurement>,
    dc: SphereAudioProcessor::DcOffset,
    bpm_min: f32,
    bpm_max: f32,
    bpm: Vec<TempoCandidate>,
    keys: Vec<KeyEstimate>,
    user_key: Option<KeyEstimate>,
    pub(super) transients: Vec<TransientMarker>,
    pub(super) transient_params: TransientDetectParams,
    freq_focus: FrequencyFocus,
    // Time/pitch
    time_mode: TimePitchMode,
    stretch_percent: f64,
    pitch_semi: f32,
    pitch_cents: f32,
    preserve_transients: bool,
    // Channel / phase / resample / repair
    channel: ChannelTransform,
    phase_ms: bool,
    phase: PhaseMeasurement,
    gonio_trail: VecDeque<(f32, f32)>,
    corr_history: VecDeque<f32>,
    envelope: Vec<f32>,
    level_history: Vec<f32>,
    ab_bank: Option<AbSnapshot>,
    ab_showing_b: bool,
    resample_target: u32,
    pub(super) denoise: SpectralDenoiseParams,
    pub(super) learned_noise: Option<Vec<f32>>,
    pub(super) declick: DeclickParams,
    pub(super) dehum: DehumParams,
    pub(super) spectral_gain_db: f32,
    spectrum_busy: bool,
    pub(super) display_smoothing: DisplaySmoothing,
    pub(super) graph_style: GraphStyle,
    pub(super) graph_hover: Option<Point<Pixels>>,
    /// Interleaved PCM of the analyzed window.
    ///
    /// Held for the lifetime of the window so the repair canvas can draw peaks
    /// and the preview worker can re-run a processor on every parameter change
    /// without decoding the file again. It costs one clip's PCM, which the
    /// analysis pass already materialized, and is released when the window
    /// closes.
    pub(super) source_pcm: Option<Arc<[f32]>>,
    pub(super) source_channels: usize,
    pub(super) repair: RepairSurface,
}

impl AudioToolWindow {
    fn new(
        session: AudioToolSession,
        timeline: Entity<Timeline>,
        callbacks: AudioToolWindowCallbacks,
        cx: &mut Context<Self>,
    ) -> Self {
        DirectAudio::analysis_tap().set_target_clip(Some(&session.target.clip_id));
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(REFRESH).await;
            if this
                .update(cx, |this, cx| {
                    this.tick_realtime(cx);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        let target_sr = session.target.sample_rate.max(44_100);
        let kind = session.tool_kind;
        let mut window = Self {
            session,
            timeline,
            callbacks,
            status: String::new(),
            spectrum_mode: SpectrumMode::SelectionAverage,
            fft_size: FftSize::N2048,
            spectrum_window: SpectrumWindow::Hann,
            smoothing: SpectrumSmoothing::None,
            peak_hold: false,
            spectrum: None,
            spectrum_avg: None,
            loudness: None,
            normalize: NormalizeParams::default(),
            measurement: None,
            dc: SphereAudioProcessor::DcOffset::default(),
            bpm_min: 60.0,
            bpm_max: 200.0,
            bpm: Vec::new(),
            keys: Vec::new(),
            user_key: None,
            transients: Vec::new(),
            transient_params: TransientDetectParams::default(),
            freq_focus: FrequencyFocus::FullBand,
            time_mode: TimePitchMode::Off,
            stretch_percent: 100.0,
            pitch_semi: 0.0,
            pitch_cents: 0.0,
            preserve_transients: true,
            channel: ChannelTransform::Stereo,
            phase_ms: false,
            phase: PhaseMeasurement::default(),
            gonio_trail: VecDeque::with_capacity(GONIO_TRAIL),
            corr_history: VecDeque::with_capacity(GONIO_TRAIL),
            envelope: Vec::new(),
            level_history: Vec::new(),
            ab_bank: None,
            ab_showing_b: false,
            resample_target: if target_sr == 44_100 { 48_000 } else { 44_100 },
            denoise: SpectralDenoiseParams::default(),
            learned_noise: None,
            declick: DeclickParams::default(),
            dehum: DehumParams::default(),
            spectral_gain_db: 0.0,
            spectrum_busy: false,
            display_smoothing: DisplaySmoothing::Light,
            graph_style: GraphStyle::FillAndLine,
            graph_hover: None,
            source_pcm: None,
            source_channels: 1,
            repair: RepairSurface::new(cx),
        };
        if !matches!(kind, AudioToolKind::SpectrogramSettings) {
            window.spawn_analyze(cx);
        }
        window
    }

    fn tick_realtime(&mut self, cx: &mut Context<Self>) {
        match self.session.tool_kind {
            AudioToolKind::SpectrumAnalyzer
                if self.spectrum_mode == SpectrumMode::RealtimePlayback && !self.spectrum_busy =>
            {
                let size = self.fft_size.size();
                let mut left = vec![0.0; size];
                let mut right = vec![0.0; size];
                let n = DirectAudio::analysis_tap().copy_recent(&mut left, &mut right);
                if n >= size / 2 {
                    left.truncate(n);
                    right.truncate(n);
                    let sr = self.session.target.sample_rate.max(44_100);
                    let fft_size = self.fft_size;
                    let window = self.spectrum_window;
                    let smoothing = self.smoothing;
                    let hold = self
                        .peak_hold
                        .then(|| self.spectrum.as_ref().map(|s| s.peak_hold_db.clone()))
                        .flatten();
                    self.spectrum_busy = true;
                    let host = cx.entity().downgrade();
                    cx.spawn(async move |_, cx| {
                        let snap = cx
                            .background_executor()
                            .spawn(async move {
                                SphereAudioProcessor::analyze_ring_window(
                                    &left,
                                    &right,
                                    sr,
                                    fft_size,
                                    window,
                                    smoothing,
                                    hold.as_deref(),
                                )
                            })
                            .await;
                        let _ = host.update(cx, |this, cx| {
                            this.spectrum_busy = false;
                            if let Some(snap) = snap {
                                this.absorb_spectrum(snap);
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                }
            }
            AudioToolKind::PhaseAnalyzer => {
                let mut left = vec![0.0; 2048];
                let mut right = vec![0.0; 2048];
                let n = DirectAudio::analysis_tap().copy_recent(&mut left, &mut right);
                if n > 16 {
                    let m = measure_phase(&left[..n], &right[..n], self.phase_ms);
                    self.push_phase(m);
                }
            }
            AudioToolKind::AudioRepair if self.session.preview_enabled => {
                self.tick_audition_level();
            }
            _ => {}
        }
    }

    fn spawn_analyze(&mut self, cx: &mut Context<Self>) {
        let kind = self.session.tool_kind;
        let path = self.session.target.source_path.clone();
        let selection = self.session.target.time_selection;
        let spectral = self.session.target.spectral_selection;
        let fft_size = self.fft_size;
        let window = self.spectrum_window;
        let smoothing = self.smoothing;
        let peak = self.spectrum_mode == SpectrumMode::SelectionPeak;
        let peak_hold = if self.peak_hold {
            self.spectrum.as_ref().map(|s| s.peak_hold_db.clone())
        } else {
            None
        };
        let bpm_min = self.bpm_min;
        let bpm_max = self.bpm_max;
        let transient_params = {
            let nyquist = self.session.target.sample_rate as f32 * 0.5;
            let (lo, hi) = self.freq_focus.band(nyquist);
            TransientDetectParams {
                freq_low_hz: lo,
                freq_high_hz: hi,
                ..self.transient_params
            }
        };
        let normalize = self.normalize;
        self.session.analyzing = true;
        self.status = "Analyzing…".to_string();
        let host = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let path = path.ok_or_else(|| "clip has no source file".to_string())?;
                    let buffer = DirectAudio::load_audio_file(&path)?;
                    let channels = buffer.channels.max(1);
                    let mut samples = buffer.samples;
                    if let Some(sel) = selection {
                        samples = slice_frames(&samples, channels, sel.start_frame, sel.end_frame);
                    }
                    let mono = downmix_interleaved(&samples, channels);
                    Ok::<_, String>((samples, mono, channels, buffer.sample_rate))
                })
                .await;
            let _ = host.update(cx, |this, cx| {
                this.session.analyzing = false;
                match result {
                    Ok((samples, mono, channels, sr)) => {
                        this.status.clear();
                        this.envelope = viz::hop_levels(&mono, LEVEL_HOPS);
                        this.level_history = this.envelope.clone();
                        this.source_channels = channels;
                        if kind == AudioToolKind::AudioRepair {
                            this.source_pcm = Some(Arc::from(samples.as_slice()));
                        }
                        match kind {
                            AudioToolKind::SpectrumAnalyzer
                            | AudioToolKind::AudioRepair
                            | AudioToolKind::SpectralProcessor
                            | AudioToolKind::Resample
                            | AudioToolKind::SpectrogramSettings => {
                                if let Some(snap) = analyze_spectrum(
                                    &mono,
                                    sr,
                                    fft_size,
                                    window,
                                    smoothing,
                                    peak,
                                    peak_hold.as_deref(),
                                ) {
                                    this.absorb_spectrum(snap);
                                }
                            }
                            AudioToolKind::Loudness | AudioToolKind::Normalize => {
                                this.loudness = analyze_loudness(&samples, channels, sr);
                                this.measurement =
                                    Some(measure_normalize(&samples, channels, sr, normalize));
                            }
                            AudioToolKind::DcOffset => {
                                this.dc = measure_dc_offset(&samples, channels);
                            }
                            AudioToolKind::BpmAnalysis => {
                                this.bpm =
                                    estimate_bpm_candidates(&mono, sr as f32, bpm_min, bpm_max);
                            }
                            AudioToolKind::KeyAnalysis => {
                                this.keys = estimate_key_ranked(&mono, sr as f32);
                            }
                            AudioToolKind::TransientDetector => {
                                this.transients = detect_transients(&mono, sr, transient_params);
                                this.status = format!("{} transients", this.transients.len());
                            }
                            AudioToolKind::PhaseAnalyzer => {
                                if channels >= 2 {
                                    let mut l = Vec::new();
                                    let mut r = Vec::new();
                                    for frame in samples.chunks(channels) {
                                        l.push(frame[0]);
                                        r.push(frame[1]);
                                    }
                                    let measured = measure_phase(&l, &r, this.phase_ms);
                                    this.push_phase(measured);
                                }
                            }
                            AudioToolKind::TimePitch | AudioToolKind::ChannelTools => {}
                        }
                        if kind == AudioToolKind::SpectralProcessor {
                            this.status = format!(
                                "region {}–{} Hz",
                                spectral.map(|s| s.min_hz).unwrap_or(0.0),
                                spectral.map(|s| s.max_hz).unwrap_or(sr as f32 * 0.5)
                            );
                        }
                        if kind == AudioToolKind::AudioRepair {
                            this.on_repair_source_ready(cx);
                        }
                    }
                    Err(error) => this.status = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Learn the noise profile from the canvas region when one is drawn, and
    /// from the whole analyzed window otherwise.
    ///
    /// Uses the buffer analysis already decoded, so learning a profile while
    /// auditioning never touches the filesystem.
    pub(super) fn spawn_learn_noise(&mut self, cx: &mut Context<Self>) {
        let Some(pcm) = self.source_pcm.clone() else {
            self.status = "Analyze the source first".to_string();
            cx.notify();
            return;
        };
        let channels = self.source_channels.max(1);
        let (start, end) = self.repair_learn_range(pcm.len() / channels);
        if end.saturating_sub(start) < 2048 {
            self.status = "Select at least 2048 frames of noise".to_string();
            cx.notify();
            return;
        }
        let learned_frames = end - start;
        let sample_rate = self.session.target.sample_rate.max(1);
        self.status = "Learning noise profile…".to_string();
        let host = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let profile = cx
                .background_executor()
                .spawn(async move {
                    let region = &pcm[start * channels..(end * channels).min(pcm.len())];
                    learn_noise_profile(&downmix_interleaved(region, channels), 2048, 512)
                })
                .await;
            let _ = host.update(cx, |this, cx| {
                this.learned_noise = Some(profile);
                this.repair.profile_frames = learned_frames as u64;
                this.status = format!(
                    "Noise profile learned from {:.2} s",
                    learned_frames as f32 / sample_rate as f32
                );
                this.invalidate_repair_preview(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn dispatch(&self, command: AudioToolCommand, cx: &mut App) {
        (self.callbacks.on_command)(command, cx);
    }

    pub(super) fn emit_preview(&mut self, cx: &mut App) {
        if !self.session.preview_enabled {
            self.dispatch(
                AudioToolCommand::ClearPreview(self.session.target.clip_id.clone()),
                cx,
            );
            return;
        }
        let mut preview = ClipPreviewOverride::identity(self.session.target.clip_id.clone());
        preview.bypass = self.session.preview_bypassed;
        match self.session.tool_kind {
            AudioToolKind::Normalize => {
                if let Some(m) = self.measurement {
                    preview.extra_gain = Some(db_to_lin(m.required_gain_db));
                }
            }
            AudioToolKind::ChannelTools => preview.channel = Some(self.channel),
            AudioToolKind::DcOffset => {
                preview.dc_remove = Some(true);
                preview.dc_left = self.dc.left;
                preview.dc_right = self.dc.right;
            }
            AudioToolKind::TimePitch => {
                preview.stretch_ratio = Some((self.stretch_percent / 100.0).clamp(0.05, 20.0));
                preview.pitch_semitones = Some(self.pitch_semi + self.pitch_cents / 100.0);
            }
            AudioToolKind::AudioRepair if self.repair.module == AudioRepairModule::DeHum => {
                preview.dehum = Some(self.dehum);
            }
            AudioToolKind::AudioRepair if self.repair.module == AudioRepairModule::Denoise => {
                preview.denoise_amount = Some((self.denoise.reduction_db / 24.0).clamp(0.0, 1.0));
            }
            _ => {}
        }
        self.dispatch(AudioToolCommand::Preview(preview), cx);
    }

    fn absorb_spectrum(&mut self, snap: SpectrumSnapshot) {
        const LIVE: f32 = 0.38;
        const AVG: f32 = 0.10;
        let live = self.spectrum_mode == SpectrumMode::RealtimePlayback
            && self
                .spectrum
                .as_ref()
                .is_some_and(|prev| prev.magnitudes_db.len() == snap.magnitudes_db.len());
        if live {
            if let Some(prev) = self.spectrum.as_mut() {
                for (dst, src) in prev.magnitudes_db.iter_mut().zip(&snap.magnitudes_db) {
                    *dst = *dst * (1.0 - LIVE) + *src * LIVE;
                }
                if prev.peak_hold_db.len() == snap.peak_hold_db.len() {
                    for (dst, src) in prev.peak_hold_db.iter_mut().zip(&snap.peak_hold_db) {
                        *dst = dst.max(*src);
                    }
                } else {
                    prev.peak_hold_db = snap.peak_hold_db.clone();
                }
                prev.sample_rate = snap.sample_rate;
                prev.fft_size = snap.fft_size;
            }
        } else {
            self.spectrum = Some(snap);
        }
        let src = self
            .spectrum
            .as_ref()
            .map(|s| s.magnitudes_db.as_slice())
            .unwrap_or(&[]);
        match self.spectrum_avg.as_mut() {
            Some(avg) if avg.len() == src.len() && live => {
                for (dst, v) in avg.iter_mut().zip(src) {
                    *dst = *dst * (1.0 - AVG) + *v * AVG;
                }
            }
            _ => self.spectrum_avg = Some(src.to_vec()),
        }
    }

    fn graph_draw(&self) -> GraphDraw {
        GraphDraw {
            smoothing: self.display_smoothing,
            style: self.graph_style,
            hover: self.graph_hover,
        }
    }

    fn tracked_plot(&self, plot: impl IntoElement, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("analyzer-plot")
            .size_full()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let pos = event.position;
                if this.graph_hover != Some(pos) {
                    this.graph_hover = Some(pos);
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.graph_hover.is_some() {
                    this.graph_hover = None;
                    cx.notify();
                }
            }))
            .child(plot)
    }

    fn push_phase(&mut self, measured: PhaseMeasurement) {
        self.phase = measured;
        self.gonio_trail
            .push_back((measured.gonio_x, measured.gonio_y));
        if self.gonio_trail.len() > GONIO_TRAIL {
            self.gonio_trail.pop_front();
        }
        self.corr_history.push_back(measured.correlation);
        if self.corr_history.len() > GONIO_TRAIL {
            self.corr_history.pop_front();
        }
    }

    fn capture_ab(&self) -> AbSnapshot {
        AbSnapshot {
            time_mode: self.time_mode,
            stretch_percent: self.stretch_percent,
            pitch_semi: self.pitch_semi,
            pitch_cents: self.pitch_cents,
            preserve_transients: self.preserve_transients,
            normalize: self.normalize,
            channel: self.channel,
            resample_target: self.resample_target,
            repair_module: self.repair.module,
            denoise: self.denoise,
            declick: self.declick,
            dehum: self.dehum,
            spectral_gain_db: self.spectral_gain_db,
            fft_size: self.fft_size,
            spectrum_window: self.spectrum_window,
            smoothing: self.smoothing,
            peak_hold: self.peak_hold,
            spectrum_mode: self.spectrum_mode,
            transient_params: self.transient_params,
            freq_focus: self.freq_focus,
            bpm_min: self.bpm_min,
            bpm_max: self.bpm_max,
        }
    }

    fn restore_ab(&mut self, snap: AbSnapshot) {
        self.time_mode = snap.time_mode;
        self.stretch_percent = snap.stretch_percent;
        self.pitch_semi = snap.pitch_semi;
        self.pitch_cents = snap.pitch_cents;
        self.preserve_transients = snap.preserve_transients;
        self.normalize = snap.normalize;
        self.channel = snap.channel;
        self.resample_target = snap.resample_target;
        self.repair.module = snap.repair_module;
        self.denoise = snap.denoise;
        self.declick = snap.declick;
        self.dehum = snap.dehum;
        self.spectral_gain_db = snap.spectral_gain_db;
        self.fft_size = snap.fft_size;
        self.spectrum_window = snap.spectrum_window;
        self.smoothing = snap.smoothing;
        self.peak_hold = snap.peak_hold;
        self.spectrum_mode = snap.spectrum_mode;
        self.transient_params = snap.transient_params;
        self.freq_focus = snap.freq_focus;
        self.bpm_min = snap.bpm_min;
        self.bpm_max = snap.bpm_max;
    }

    fn toggle_ab(&mut self, cx: &mut App) {
        if let Some(bank) = self.ab_bank.take() {
            let current = self.capture_ab();
            self.restore_ab(bank);
            self.ab_bank = Some(current);
            self.ab_showing_b = !self.ab_showing_b;
            self.emit_preview(cx);
        } else {
            self.ab_bank = Some(self.capture_ab());
            self.ab_showing_b = false;
        }
        self.session.dirty = true;
    }

    fn needs_analyze(kind: AudioToolKind) -> bool {
        !matches!(kind, AudioToolKind::ChannelTools | AudioToolKind::TimePitch)
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        let clip_id = self.session.target.clip_id.clone();
        match self.session.tool_kind {
            AudioToolKind::Normalize => {
                let Some(m) = self.measurement else {
                    self.status = "Analyze first".to_string();
                    return;
                };
                let gain_db = m.required_gain_db;
                self.apply_offline_pcm(cx, move |samples, _channels, _sr| {
                    let mut out = samples.to_vec();
                    apply_gain_interleaved(&mut out, gain_db);
                    Ok(out)
                });
                return;
            }
            AudioToolKind::ChannelTools => {
                let channel = self.channel;
                self.apply_offline_pcm(cx, move |samples, channels, _sr| {
                    Ok(apply_channel_transform_interleaved(
                        samples, channels, channel,
                    ))
                });
                return;
            }
            AudioToolKind::DcOffset => {
                let dc = self.dc;
                self.apply_offline_pcm(cx, move |samples, _channels, _sr| {
                    let mut processor = DcOffsetProcessor::new(dc, true);
                    let mut out = vec![0.0; samples.len()];
                    processor.process(samples, &mut out);
                    Ok(out)
                });
                return;
            }
            AudioToolKind::TimePitch => {
                let percent = self.stretch_percent;
                let semi = self.pitch_semi;
                let cents = self.pitch_cents;
                let mode = self.time_mode;
                let time_ratio = match mode {
                    TimePitchMode::Off => 1.0,
                    TimePitchMode::FollowTempo
                    | TimePitchMode::Stretch
                    | TimePitchMode::FitDuration => (percent / 100.0).clamp(0.05, 20.0) as f32,
                };
                let pitch_ratio = semitone_to_pitch_ratio(semi, cents);
                if (time_ratio - 1.0).abs() < 1.0e-4 && (pitch_ratio - 1.0).abs() < 1.0e-4 {
                    self.status = "Nothing to apply".to_string();
                    return;
                }
                self.apply_offline_pcm(cx, move |samples, channels, sr| {
                    let params = StretchParams {
                        mode: StretchMode::Manual,
                        algorithm: StretchAlgorithm::PreservePitch,
                        time_ratio,
                        pitch_ratio,
                        preserve_pitch: true,
                        quality: 0.75,
                        ..StretchParams::default()
                    };
                    render_stretch_interleaved(samples, channels, sr, &params)
                        .map_err(|error| error.to_string())
                });
                return;
            }
            AudioToolKind::AudioRepair if self.repair.module == AudioRepairModule::DeHum => {
                let params = self.dehum;
                self.apply_offline_pcm(cx, move |samples, _channels, sr| {
                    let mut processor = DehumProcessor::new(sr, params);
                    processor.reset();
                    let mut out = vec![0.0; samples.len()];
                    processor.process(samples, &mut out);
                    Ok(out)
                });
                return;
            }
            AudioToolKind::AudioRepair if self.repair.module == AudioRepairModule::Denoise => {
                if let Some(profile) = self.learned_noise.clone() {
                    let params = self.denoise;
                    self.apply_offline_pcm(cx, move |samples, channels, sr| {
                        let mono = downmix_interleaved(samples, channels);
                        let out_mono = reduce_noise_stft(&mono, sr, &profile, params);
                        let mut out = Vec::with_capacity(out_mono.len() * channels);
                        for sample in out_mono {
                            for _ in 0..channels {
                                out.push(sample);
                            }
                        }
                        Ok(out)
                    });
                    return;
                }
                let amount = (self.denoise.reduction_db / 24.0).clamp(0.0, 1.0);
                self.dispatch(
                    AudioToolCommand::MutateClip {
                        clip_id: clip_id.clone(),
                        label: "Noise Reduction",
                        mutate: Box::new(move |clip| {
                            clip.stretch.denoise_amount = amount;
                        }),
                    },
                    cx,
                );
            }
            AudioToolKind::AudioRepair if self.repair.module == AudioRepairModule::DeClick => {
                let params = self.declick;
                self.apply_offline_pcm(cx, move |samples, channels, _sr| {
                    Ok(declick_interleaved(samples, channels, params).0)
                });
                return;
            }
            AudioToolKind::AudioRepair
                if self.repair.module == AudioRepairModule::SpectralRepair =>
            {
                self.apply_spectral_region(cx);
                return;
            }
            AudioToolKind::SpectralProcessor => {
                self.apply_spectral_region(cx);
                return;
            }
            AudioToolKind::Resample => {
                let target = self.resample_target;
                self.apply_offline_pcm(cx, move |samples, channels, sr| {
                    resample_interleaved(samples, channels, sr, target).map_err(|e| e.to_string())
                });
                return;
            }
            _ => {}
        }
        self.dispatch(AudioToolCommand::ClearPreview(clip_id), cx);
        self.session.preview_enabled = false;
        self.session.dirty = false;
    }

    /// Spectral gain over the targeted time-frequency region. Shared by the
    /// standalone Spectral Processing tool and the repair surface's Spectral
    /// Repair module so both commit identical audio.
    fn apply_spectral_region(&mut self, cx: &mut Context<Self>) {
        let gain = db_to_lin(self.spectral_gain_db);
        let Some(sel) = self.spectral_region() else {
            self.status = "Select a time-frequency region first".to_string();
            return;
        };
        let (window_start, _) = self.clip_source_window(cx);
        let window_start = window_start as i64;
        self.apply_offline_pcm(cx, move |samples, channels, sr| {
            let mono = downmix_interleaved(samples, channels);
            let params = SpectralGainParams {
                start_frame: (sel.start_frame - window_start).max(0),
                end_frame: (sel.end_frame - window_start).max(0),
                min_hz: sel.min_hz,
                max_hz: sel.max_hz,
                gain,
                fade_bins: 4,
            };
            let out_mono = apply_spectral_gain(&mono, sr, params, StftSettings::default());
            let mut out = Vec::with_capacity(out_mono.len() * channels);
            for sample in out_mono {
                for _ in 0..channels {
                    out.push(sample);
                }
            }
            Ok(out)
        });
    }

    /// The time-frequency region a spectral commit writes into. The repair
    /// surface prefers its own canvas region so the drawn box is what gets
    /// processed.
    fn spectral_region(&self) -> Option<SpectralSelection> {
        if self.session.tool_kind == AudioToolKind::AudioRepair {
            if let Some(region) = self.repair.region {
                return Some(SpectralSelection {
                    start_frame: region.start_frame,
                    end_frame: region.end_frame,
                    min_hz: region.min_hz,
                    max_hz: region.max_hz,
                });
            }
        }
        self.session.target.spectral_selection
    }

    /// The time range a commit writes into.
    ///
    /// On the repair surface a canvas drag defines the range, so what the user
    /// framed on screen is exactly what gets processed. Everything else keeps
    /// using the host's time selection.
    fn processing_range(&self) -> Option<AudioRangeSelection> {
        if self.session.tool_kind == AudioToolKind::AudioRepair {
            if let Some(region) = self.repair.region {
                return Some(AudioRangeSelection {
                    start_frame: region.start_frame,
                    end_frame: region.end_frame,
                });
            }
        }
        self.session.target.time_selection
    }

    /// Whether the active processor reframes the whole clip rather than
    /// replacing a range inside it.
    fn processes_whole_clip(&self) -> bool {
        match self.session.tool_kind {
            AudioToolKind::TimePitch
            | AudioToolKind::Resample
            | AudioToolKind::SpectralProcessor => true,
            // Spectral gain addresses absolute frames itself, so handing it a
            // pre-sliced region would shift its own window.
            AudioToolKind::AudioRepair => self.repair.module == AudioRepairModule::SpectralRepair,
            _ => false,
        }
    }

    fn clip_source_window(&self, cx: &App) -> (u64, u64) {
        self.timeline
            .read(cx)
            .state
            .find_clip(&self.session.target.clip_id)
            .map(|(_, clip)| {
                let start = clip.stretch.source_start_samples;
                let end = if clip.stretch.source_end_samples > start {
                    clip.stretch.source_end_samples
                } else {
                    clip.stretch.original_duration_samples
                };
                (start, end)
            })
            .unwrap_or((0, 0))
    }

    fn apply_offline_pcm(
        &mut self,
        cx: &mut Context<Self>,
        process: impl FnOnce(&[f32], usize, u32) -> Result<Vec<f32>, String> + Send + 'static,
    ) {
        let clip_id = self.session.target.clip_id.clone();
        let path = self.session.target.source_path.clone();
        let selection = self.processing_range();
        let tool_kind = self.session.tool_kind;
        let process_whole_clip = self.processes_whole_clip();
        let (source_start, source_end) = self.clip_source_window(cx);
        let target_rate = if tool_kind == AudioToolKind::Resample {
            self.resample_target
        } else {
            self.session.target.sample_rate
        };
        self.status = "Rendering…".to_string();
        let host = cx.entity().downgrade();
        let on_command = self.callbacks.on_command.clone();
        let path_clip_id = clip_id.clone();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let path = path.ok_or_else(|| "clip has no source file".to_string())?;
                    let buffer = DirectAudio::load_audio_file(&path)?;
                    let channels = buffer.channels.max(1);
                    let total_frames = (buffer.samples.len() / channels.max(1)) as u64;
                    let window_start = source_start.min(total_frames);
                    let window_end = if source_end > window_start {
                        source_end.min(total_frames)
                    } else {
                        total_frames
                    };
                    // Bounce this clip's audible window only. A shared source
                    // (duplicates, split halves) must keep the original file so
                    // the other clips do not inherit a crop.
                    let mut clip_samples = slice_frames(
                        &buffer.samples,
                        channels,
                        window_start as i64,
                        window_end as i64,
                    );
                    if clip_samples.is_empty() {
                        return Err("clip source window is empty".to_string());
                    }
                    if process_whole_clip {
                        clip_samples = process(&clip_samples, channels, buffer.sample_rate)?;
                    } else if let Some(sel) = selection {
                        let local_start =
                            (sel.start_frame.max(0) as u64).saturating_sub(window_start);
                        let local_end = (sel.end_frame.max(0) as u64)
                            .saturating_sub(window_start)
                            .max(local_start);
                        let region = slice_frames(
                            &clip_samples,
                            channels,
                            local_start as i64,
                            local_end as i64,
                        );
                        if !region.is_empty() {
                            let processed = process(&region, channels, buffer.sample_rate)?;
                            replace_frame_range(
                                &mut clip_samples,
                                channels,
                                local_start as i64,
                                &processed,
                            );
                        }
                    } else {
                        clip_samples = process(&clip_samples, channels, buffer.sample_rate)?;
                    }
                    let out_path =
                        processed_output_path(std::path::Path::new(&path), &path_clip_id);
                    write_wav_f32(&out_path, &clip_samples, channels as u16, target_rate)
                        .map_err(|e| e.to_string())?;
                    Ok::<_, String>(out_path)
                })
                .await;
            let _ = host.update(cx, |this, cx| {
                match result {
                    Ok(path) => {
                        (on_command)(
                            AudioToolCommand::ReplaceSource {
                                clip_id: clip_id.clone(),
                                path,
                                sample_rate: target_rate,
                            },
                            cx,
                        );
                        this.status = "Applied".to_string();
                        this.dispatch(AudioToolCommand::ClearPreview(clip_id), cx);
                    }
                    Err(error) => this.status = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut App) {
        self.dispatch(
            AudioToolCommand::ClearPreview(self.session.target.clip_id.clone()),
            cx,
        );
        self.session.preview_enabled = false;
        self.session.dirty = false;
    }
}

fn sanitized_clip_id(clip_id: &str) -> String {
    let sanitized: String = clip_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "clip".to_string()
    } else {
        sanitized
    }
}

/// Derived bounce next to the source, unique per clip so a split sibling or
/// duplicate cannot be overwritten. Re-applying the same clip replaces its
/// own file instead of stacking suffixes.
fn processed_output_path(source: &std::path::Path, clip_id: &str) -> PathBuf {
    let mut out = source.to_path_buf();
    let stem = out
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("clip");
    let clip_tag = sanitized_clip_id(clip_id);
    let suffix = format!(".{clip_tag}.processed");
    let stem = stem.strip_suffix(&suffix).unwrap_or(stem);
    let stem = stem.strip_suffix("-processed").unwrap_or(stem);
    out.set_file_name(format!("{stem}.{clip_tag}.processed.wav"));
    out
}

fn bind_f32(
    cx: &mut Context<AudioToolWindow>,
    write: impl Fn(&mut AudioToolWindow, f32, &mut Context<AudioToolWindow>) + 'static,
) -> impl Fn(f32, &mut Window, &mut App) + 'static {
    let handle = cx.entity();
    move |value, _window, cx| {
        handle.update(cx, |this, cx| write(this, value, cx));
    }
}

fn tonic_index(tonic: SphereAudioProcessor::PitchClass) -> usize {
    use SphereAudioProcessor::PitchClass::*;
    match tonic {
        C => 0,
        Cs => 1,
        D => 2,
        Ds => 3,
        E => 4,
        F => 5,
        Fs => 6,
        G => 7,
        Gs => 8,
        A => 9,
        As => 10,
        B => 11,
    }
}

impl Render for AudioToolWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = self.session.tool_kind;
        let analysis = kind.is_analysis_only();
        let on_close = self.callbacks.on_close.clone();
        let target = self.session.target.clone();
        let status = if self.session.analyzing {
            format!("Analyzing… {:.0}%", self.session.analyze_progress * 100.0)
        } else {
            self.status.clone()
        };
        let ab_label = if self.ab_bank.is_none() {
            "A/B"
        } else if self.ab_showing_b {
            "B"
        } else {
            "A"
        };
        // The repair surface is a workspace, not a dialog: it auditions
        // continuously and commits, where the single-purpose tools preview and
        // apply. Same controls, vocabulary matched to the interaction.
        let repair = kind == AudioToolKind::AudioRepair;
        let (audition_label, discard_label, commit_label) = if repair {
            ("Audition", "Discard", "Commit")
        } else {
            ("Preview", "Cancel", "Apply")
        };
        // Offline-only repair modules have no realtime path in the engine, so
        // the latch stays disabled rather than implying playback changed.
        let audition_available = !repair || self.repair_audition_enabled();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(crate::theme::ui_font())
            .child(external_window_titlebar(
                kind.label(),
                "audio-tool-close",
                move |window, cx| {
                    on_close(kind, window.bounds(), cx);
                    window.remove_window();
                },
            ))
            .child(workspace::header(
                target.target_label(kind),
                target.summary_line(),
                status,
                workspace::latch(
                    "follow",
                    "Follow",
                    self.session.follow_selection,
                    true,
                    cx.listener(|this, _, _, cx| {
                        this.session.follow_selection = !this.session.follow_selection;
                        this.session.pin_target = !this.session.follow_selection;
                        cx.notify();
                    }),
                ),
                workspace::latch(
                    "pin",
                    "Pin",
                    self.session.pin_target,
                    true,
                    cx.listener(|this, _, _, cx| {
                        this.session.pin_target = !this.session.pin_target;
                        this.session.follow_selection = !this.session.pin_target;
                        cx.notify();
                    }),
                ),
            ))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .child(self.tool_body(cx)),
            )
            .child(workspace::workflow_bar(
                workspace::workflow_row()
                    .when(Self::needs_analyze(kind), |row| {
                        row.child(workspace::ghost_action(
                            "analyze",
                            "Analyze",
                            !self.session.analyzing,
                            cx.listener(|this, _, _, cx| this.spawn_analyze(cx)),
                        ))
                    })
                    .when(!analysis, |row| {
                        row.child(workspace::latch(
                            "preview",
                            audition_label,
                            self.session.preview_enabled,
                            audition_available,
                            cx.listener(|this, _, _, cx| {
                                this.session.preview_enabled = !this.session.preview_enabled;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                        ))
                        .child(workspace::latch(
                            "bypass",
                            "Bypass",
                            self.session.preview_bypassed,
                            self.session.preview_enabled && audition_available,
                            cx.listener(|this, _, _, cx| {
                                this.session.preview_bypassed = !this.session.preview_bypassed;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                        ))
                        .child(workspace::latch(
                            "ab",
                            ab_label,
                            self.ab_bank.is_some(),
                            true,
                            cx.listener(|this, _, _, cx| {
                                this.toggle_ab(cx);
                                cx.notify();
                            }),
                        ))
                    })
                    .when(repair && self.session.preview_enabled, |row| {
                        row.child(self.audition_meter())
                    })
                    .child(div().flex_1())
                    .when(!analysis, |row| {
                        row.child(workspace::ghost_action(
                            "cancel",
                            discard_label,
                            true,
                            cx.listener(|this, _, window, cx| {
                                this.cancel(cx);
                                (this.callbacks.on_close)(
                                    this.session.tool_kind,
                                    window.bounds(),
                                    cx,
                                );
                                window.remove_window();
                            }),
                        ))
                        .child(workspace::apply_action(
                            "apply",
                            commit_label,
                            true,
                            cx.listener(|this, _, _, cx| {
                                this.apply(cx);
                                cx.notify();
                            }),
                        ))
                    }),
            ))
    }
}

impl AudioToolWindow {
    fn tool_body(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        match self.session.tool_kind {
            AudioToolKind::SpectrumAnalyzer => self.spectrum_body(cx).into_any_element(),
            AudioToolKind::Loudness => self.loudness_body(cx).into_any_element(),
            AudioToolKind::Normalize => self.normalize_body(cx).into_any_element(),
            AudioToolKind::TransientDetector => self.transient_body(cx).into_any_element(),
            AudioToolKind::TimePitch => self.time_pitch_body(cx).into_any_element(),
            AudioToolKind::Resample => self.resample_body(cx).into_any_element(),
            AudioToolKind::ChannelTools => self.channel_body(cx).into_any_element(),
            AudioToolKind::PhaseAnalyzer => self.phase_body(cx).into_any_element(),
            AudioToolKind::DcOffset => self.dc_body(cx).into_any_element(),
            AudioToolKind::BpmAnalysis => self.bpm_body(cx).into_any_element(),
            AudioToolKind::KeyAnalysis => self.key_body(cx).into_any_element(),
            AudioToolKind::AudioRepair => self.repair_surface(cx).into_any_element(),
            AudioToolKind::SpectralProcessor => self.spectral_body(cx).into_any_element(),
            AudioToolKind::SpectrogramSettings => {
                self.spectrogram_settings_body(cx).into_any_element()
            }
        }
    }

    fn spectrum_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.spectrum.as_ref();
        let mag = snap.map(|s| s.magnitudes_db.as_slice()).unwrap_or(&[]);
        let hold = if self.peak_hold || self.graph_style == GraphStyle::Overlay {
            snap.map(|s| s.peak_hold_db.as_slice()).unwrap_or(&[])
        } else {
            &[]
        };
        let avg = self.spectrum_avg.as_deref().unwrap_or(&[]);
        let sr = snap
            .map(|s| s.sample_rate)
            .unwrap_or(self.session.target.sample_rate);
        let overlay = {
            let mut stack = workspace::overlay_stack();
            stack = stack.child(workspace::overlay_line(self.spectrum_mode.label()));
            if let Some(snap) = snap {
                if let Some((bin, db)) = snap
                    .magnitudes_db
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                {
                    let hz = bin as f32 * snap.sample_rate as f32 / snap.fft_size.max(1) as f32;
                    stack = stack.child(workspace::overlay_line(format!(
                        "Peak {hz:.0} Hz  {db:.1} dB"
                    )));
                }
            } else {
                stack = stack.child(workspace::overlay_line("Analyze or play to fill"));
            }
            stack
        };
        workspace::stage(
            workspace::viz_frame(
                self.tracked_plot(
                    viz::spectrum_view(mag, avg, hold, sr, self.graph_draw()),
                    cx,
                ),
                overlay,
            ),
            workspace::control_strip(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .child(self.spectrum_mode_track(cx))
                    .child(self.fft_track(cx))
                    .child(
                        div()
                            .flex()
                            .gap(px(space::TIGHT))
                            .child(self.window_track(cx))
                            .child(self.smooth_track(cx))
                            .child(workspace::latch(
                                "peak-hold",
                                "Hold",
                                self.peak_hold,
                                true,
                                cx.listener(|this, _, _, cx| {
                                    this.peak_hold = !this.peak_hold;
                                    cx.notify();
                                }),
                            )),
                    )
                    .child(self.graph_draw_tracks(cx)),
            ),
        )
    }

    fn spectrum_mode_track(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let modes = [
            (SpectrumMode::RealtimePlayback, "Live"),
            (SpectrumMode::SelectionAverage, "Avg"),
            (SpectrumMode::SelectionPeak, "Peak"),
            (SpectrumMode::StaticCursor, "Cursor"),
        ];
        let count = modes.len();
        workspace::segment_track().children(modes.into_iter().enumerate().map(
            |(index, (mode, label))| {
                workspace::compact_segment(
                    format!("spec-mode-{label}"),
                    label,
                    self.spectrum_mode == mode,
                    workspace::segment_position(index, count),
                    cx.listener(move |this, _, _, cx| {
                        this.spectrum_mode = mode;
                        if mode != SpectrumMode::RealtimePlayback {
                            this.spawn_analyze(cx);
                        }
                        cx.notify();
                    }),
                )
            },
        ))
    }

    fn fft_track(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = FftSize::ALL.len();
        workspace::segment_track().children(FftSize::ALL.into_iter().enumerate().map(
            |(index, size)| {
                workspace::compact_segment(
                    format!("fft-{}", size.label()),
                    size.label(),
                    self.fft_size == size,
                    workspace::segment_position(index, count),
                    cx.listener(move |this, _, _, cx| {
                        this.fft_size = size;
                        cx.notify();
                    }),
                )
            },
        ))
    }

    fn window_track(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let windows = [SpectrumWindow::Hann, SpectrumWindow::BlackmanHarris];
        let count = windows.len();
        workspace::segment_track().children(windows.into_iter().enumerate().map(
            |(index, window)| {
                workspace::compact_segment(
                    format!("win-{}", window.label()),
                    window.label(),
                    self.spectrum_window == window,
                    workspace::segment_position(index, count),
                    cx.listener(move |this, _, _, cx| {
                        this.spectrum_window = window;
                        cx.notify();
                    }),
                )
            },
        ))
    }

    fn smooth_track(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let options = [
            (SpectrumSmoothing::None, "Off"),
            (SpectrumSmoothing::TwelfthOctave, "1/12"),
            (SpectrumSmoothing::SixthOctave, "1/6"),
            (SpectrumSmoothing::ThirdOctave, "1/3"),
        ];
        let count = options.len();
        workspace::segment_track().children(options.into_iter().enumerate().map(
            |(index, (value, label))| {
                workspace::compact_segment(
                    format!("smooth-{label}"),
                    label,
                    self.smoothing == value,
                    workspace::segment_position(index, count),
                    cx.listener(move |this, _, _, cx| {
                        this.smoothing = value;
                        cx.notify();
                    }),
                )
            },
        ))
    }

    fn graph_draw_tracks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap(px(space::TIGHT))
            .child(self.display_smooth_track(cx))
            .child(self.graph_style_track(cx))
    }

    fn display_smooth_track(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = DisplaySmoothing::ALL.len();
        workspace::segment_track().children(DisplaySmoothing::ALL.into_iter().enumerate().map(
            |(index, value)| {
                workspace::compact_segment(
                    format!("ds-{}", value.label()),
                    value.label(),
                    self.display_smoothing == value,
                    workspace::segment_position(index, count),
                    cx.listener(move |this, _, _, cx| {
                        this.display_smoothing = value;
                        cx.notify();
                    }),
                )
            },
        ))
    }

    fn graph_style_track(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = GraphStyle::ALL.len();
        workspace::segment_track().children(GraphStyle::ALL.into_iter().enumerate().map(
            |(index, value)| {
                workspace::compact_segment(
                    format!("gs-{}", value.label()),
                    value.label(),
                    self.graph_style == value,
                    workspace::segment_position(index, count),
                    cx.listener(move |this, _, _, cx| {
                        this.graph_style = value;
                        cx.notify();
                    }),
                )
            },
        ))
    }

    fn loudness_body(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let m = self.loudness;
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(
                m.map(|v| format!("M {:+.1} LUFS", v.momentary_lufs))
                    .unwrap_or_else(|| "M —".into()),
            ))
            .child(workspace::overlay_line(
                m.map(|v| format!("S {:+.1} LUFS", v.shortterm_lufs))
                    .unwrap_or_else(|| "S —".into()),
            ))
            .child(workspace::overlay_line(
                m.map(|v| format!("I {:+.1} LUFS", v.integrated_lufs))
                    .unwrap_or_else(|| "I —".into()),
            ))
            .child(workspace::overlay_line(
                m.map(|v| {
                    format!(
                        "LRA {:.1} LU  TP {:+.1} dBTP",
                        v.loudness_range, v.true_peak_dbtp
                    )
                })
                .unwrap_or_else(|| "LRA —".into()),
            ));
        workspace::stage(
            workspace::viz_frame(
                viz::loudness_view(
                    &self.level_history,
                    m.map(|v| v.momentary_lufs).unwrap_or(-70.0),
                    m.map(|v| v.shortterm_lufs).unwrap_or(-70.0),
                    m.map(|v| v.integrated_lufs).unwrap_or(-70.0),
                    m.map(|v| v.true_peak_dbtp).unwrap_or(-70.0),
                    None,
                ),
                overlay,
            ),
            workspace::control_strip(div().child(workspace::overlay_line(
                "Momentary · Short-term · Integrated · True Peak",
            ))),
        )
    }

    fn normalize_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let m = self.measurement;
        let current = match self.normalize.mode {
            NormalizeMode::Peak => m.map(|v| v.peak_dbfs).unwrap_or(-70.0),
            NormalizeMode::TruePeak => m.map(|v| v.true_peak_dbtp).unwrap_or(-70.0),
            NormalizeMode::Loudness => m.and_then(|v| v.lufs_i).unwrap_or(-70.0),
        };
        let target = match self.normalize.mode {
            NormalizeMode::Peak => self.normalize.target_peak_dbfs,
            NormalizeMode::TruePeak => self.normalize.target_true_peak_dbtp,
            NormalizeMode::Loudness => self.normalize.target_lufs,
        };
        let after = if m.is_some() { target } else { current };
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(self.normalize.mode.label()))
            .child(workspace::overlay_line(format!("Target {target:.1}")))
            .child(workspace::overlay_line(
                m.map(|v| format!("Gain {:+.1} dB", v.required_gain_db))
                    .unwrap_or_else(|| "Analyze to measure".into()),
            ));
        let modes = [
            (NormalizeMode::Peak, "Peak"),
            (NormalizeMode::TruePeak, "True Peak"),
            (NormalizeMode::Loudness, "Loudness"),
        ];
        let count = modes.len();
        workspace::stage(
            workspace::viz_frame(viz::before_after_bars(current, after, target), overlay),
            workspace::control_strip(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .child(
                        workspace::segment_track().children(modes.into_iter().enumerate().map(
                            |(index, (mode, label))| {
                                workspace::compact_segment(
                                    format!("norm-{label}"),
                                    label,
                                    self.normalize.mode == mode,
                                    workspace::segment_position(index, count),
                                    cx.listener(move |this, _, _, cx| {
                                        this.normalize.mode = mode;
                                        cx.notify();
                                    }),
                                )
                            },
                        )),
                    )
                    .child(workspace::param_row(
                        "Peak",
                        workspace::unipolar_slider(
                            "norm-peak",
                            self.normalize.target_peak_dbfs,
                            -24.0,
                            0.0,
                            bind_f32(cx, |this, value, cx| {
                                this.normalize.target_peak_dbfs = value;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                            Some(-1.0),
                        ),
                        format!("{:.1} dB", self.normalize.target_peak_dbfs),
                    ))
                    .child(workspace::param_row(
                        "True Peak",
                        workspace::unipolar_slider(
                            "norm-tp",
                            self.normalize.target_true_peak_dbtp,
                            -24.0,
                            0.0,
                            bind_f32(cx, |this, value, cx| {
                                this.normalize.target_true_peak_dbtp = value;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                            Some(-1.0),
                        ),
                        format!("{:.1} dB", self.normalize.target_true_peak_dbtp),
                    ))
                    .child(workspace::param_row(
                        "Loudness",
                        workspace::unipolar_slider(
                            "norm-lufs",
                            self.normalize.target_lufs,
                            -24.0,
                            -6.0,
                            bind_f32(cx, |this, value, cx| {
                                this.normalize.target_lufs = value;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                            Some(-14.0),
                        ),
                        format!("{:.1} LUFS", self.normalize.target_lufs),
                    )),
            ),
        )
    }

    fn transient_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let frames = self
            .session
            .target
            .time_selection
            .map(|s| (s.end_frame - s.start_frame).max(1) as f32)
            .unwrap_or(self.session.target.source_frames.max(1) as f32);
        let markers: Vec<(f32, f32)> = self
            .transients
            .iter()
            .map(|m| ((m.source_frame as f32 / frames).clamp(0.0, 1.0), m.strength))
            .collect();
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(format!(
                "{} transients",
                self.transients.len()
            )))
            .child(workspace::overlay_line(format!(
                "Sens {:.0}%  Gap {:.0} ms",
                self.transient_params.sensitivity * 100.0,
                self.transient_params.min_gap_ms
            )));
        let foci = [
            (FrequencyFocus::FullBand, "Full"),
            (FrequencyFocus::Low, "Low"),
            (FrequencyFocus::Mid, "Mid"),
            (FrequencyFocus::High, "High"),
        ];
        let count = foci.len();
        workspace::stage(
            workspace::viz_frame(
                viz::envelope_markers_view(&self.envelope, &markers, None),
                overlay,
            ),
            workspace::control_strip(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .child(workspace::param_row(
                        "Sensitivity",
                        workspace::unipolar_slider(
                            "tr-sens",
                            self.transient_params.sensitivity,
                            0.05,
                            1.0,
                            bind_f32(cx, |this, value, cx| {
                                this.transient_params.sensitivity = value;
                                cx.notify();
                            }),
                            Some(0.65),
                        ),
                        format!("{:.0}%", self.transient_params.sensitivity * 100.0),
                    ))
                    .child(workspace::param_row(
                        "Min Gap",
                        workspace::unipolar_slider(
                            "tr-gap",
                            self.transient_params.min_gap_ms,
                            1.0,
                            200.0,
                            bind_f32(cx, |this, value, cx| {
                                this.transient_params.min_gap_ms = value;
                                cx.notify();
                            }),
                            Some(20.0),
                        ),
                        format!("{:.0} ms", self.transient_params.min_gap_ms),
                    ))
                    .child(
                        workspace::segment_track().children(foci.into_iter().enumerate().map(
                            |(index, (focus, label))| {
                                workspace::compact_segment(
                                    format!("tr-focus-{label}"),
                                    label,
                                    self.freq_focus == focus,
                                    workspace::segment_position(index, count),
                                    cx.listener(move |this, _, _, cx| {
                                        this.freq_focus = focus;
                                        cx.notify();
                                    }),
                                )
                            },
                        )),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(space::HAIR))
                            .child(inspector_mini_button(
                                "tr-markers",
                                "Markers",
                                !self.transients.is_empty(),
                                cx.listener(|this, _, _, cx| {
                                    let sr = this.session.target.sample_rate.max(1) as f64;
                                    let start = this
                                        .timeline
                                        .read(cx)
                                        .state
                                        .find_clip(&this.session.target.clip_id)
                                        .map(|(_, c)| c.start_beat as f64)
                                        .unwrap_or(0.0);
                                    let spb =
                                        this.timeline.read(cx).state.seconds_per_beat() as f64;
                                    let beats = this
                                        .transients
                                        .iter()
                                        .map(|m| {
                                            start + (m.source_frame as f64 / sr) / spb.max(1.0e-6)
                                        })
                                        .collect();
                                    this.dispatch(
                                        AudioToolCommand::AddMarkers {
                                            beats,
                                            label: "Add Transient Markers",
                                        },
                                        cx,
                                    );
                                }),
                            ))
                            .child(inspector_mini_button(
                                "tr-warp",
                                "Warp",
                                !self.transients.is_empty(),
                                cx.listener(|this, _, _, cx| {
                                    this.dispatch(
                                        AudioToolCommand::AddWarpMarkers {
                                            clip_id: this.session.target.clip_id.clone(),
                                            frames: this
                                                .transients
                                                .iter()
                                                .map(|m| m.source_frame)
                                                .collect(),
                                        },
                                        cx,
                                    );
                                }),
                            ))
                            .child(inspector_mini_button(
                                "tr-slice",
                                "Slice",
                                !self.transients.is_empty(),
                                cx.listener(|this, _, _, cx| {
                                    let sr = this.session.target.sample_rate.max(1) as f32;
                                    let start = this
                                        .timeline
                                        .read(cx)
                                        .state
                                        .find_clip(&this.session.target.clip_id)
                                        .map(|(_, c)| c.start_beat)
                                        .unwrap_or(0.0);
                                    let spb = this.timeline.read(cx).state.seconds_per_beat();
                                    let beats = this
                                        .transients
                                        .iter()
                                        .map(|m| {
                                            start + (m.source_frame as f32 / sr) / spb.max(1.0e-6)
                                        })
                                        .collect();
                                    this.dispatch(
                                        AudioToolCommand::SliceClip {
                                            clip_id: this.session.target.clip_id.clone(),
                                            beats,
                                        },
                                        cx,
                                    );
                                }),
                            ))
                            .child(inspector_mini_button(
                                "tr-quant",
                                "Quantize",
                                false,
                                |_, _, _| {},
                            )),
                    ),
            ),
        )
    }

    fn time_pitch_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ratio = (self.stretch_percent / 100.0) as f32;
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(format!(
                "Time {:.1}%",
                self.stretch_percent
            )))
            .child(workspace::overlay_line(format!(
                "Pitch {:+.2} st",
                self.pitch_semi + self.pitch_cents / 100.0
            )));
        let modes = [
            (TimePitchMode::Off, "Off"),
            (TimePitchMode::Stretch, "Stretch"),
            (TimePitchMode::FitDuration, "Fit"),
            (TimePitchMode::FollowTempo, "Tempo"),
        ];
        let count = modes.len();
        workspace::stage(
            workspace::viz_frame(
                viz::time_pitch_view(ratio, self.pitch_semi + self.pitch_cents / 100.0),
                overlay,
            ),
            workspace::control_strip(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .child(
                        workspace::segment_track().children(modes.into_iter().enumerate().map(
                            |(index, (mode, label))| {
                                workspace::compact_segment(
                                    format!("tp-{label}"),
                                    label,
                                    self.time_mode == mode,
                                    workspace::segment_position(index, count),
                                    cx.listener(move |this, _, _, cx| {
                                        this.time_mode = mode;
                                        if mode == TimePitchMode::Off {
                                            this.stretch_percent = 100.0;
                                        }
                                        this.emit_preview(cx);
                                        cx.notify();
                                    }),
                                )
                            },
                        )),
                    )
                    .child(workspace::param_row(
                        "Stretch",
                        workspace::unipolar_slider(
                            "tp-stretch",
                            self.stretch_percent as f32,
                            25.0,
                            400.0,
                            bind_f32(cx, |this, value, cx| {
                                this.stretch_percent = value as f64;
                                this.time_mode = TimePitchMode::Stretch;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                            Some(100.0),
                        ),
                        format!("{:.1}%", self.stretch_percent),
                    ))
                    .child(workspace::param_row(
                        "Semitones",
                        workspace::unipolar_slider(
                            "tp-semi",
                            self.pitch_semi,
                            -24.0,
                            24.0,
                            bind_f32(cx, |this, value, cx| {
                                this.pitch_semi = value;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                            Some(0.0),
                        ),
                        format!("{:+.1}", self.pitch_semi),
                    ))
                    .child(workspace::param_row(
                        "Cents",
                        workspace::unipolar_slider(
                            "tp-cents",
                            self.pitch_cents,
                            -50.0,
                            50.0,
                            bind_f32(cx, |this, value, cx| {
                                this.pitch_cents = value;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                            Some(0.0),
                        ),
                        format!("{:+.0}", self.pitch_cents),
                    ))
                    .child(workspace::latch(
                        "tp-trans",
                        "Preserve Transients",
                        self.preserve_transients,
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.preserve_transients = !this.preserve_transients;
                            cx.notify();
                        }),
                    )),
            ),
        )
    }

    fn resample_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mag = self
            .spectrum
            .as_ref()
            .map(|s| s.magnitudes_db.as_slice())
            .unwrap_or(&[]);
        let current = self.session.target.sample_rate as f32;
        let target = self.resample_target as f32;
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(format!("Now {current:.0} Hz")))
            .child(workspace::overlay_line(format!("Nyquist → {target:.0} Hz")));
        let rates = [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000];
        let count = rates.len();
        workspace::stage(
            workspace::viz_frame(
                self.tracked_plot(
                    viz::resample_view(current, target, mag, self.graph_draw()),
                    cx,
                ),
                overlay,
            ),
            workspace::control_strip(workspace::segment_track().children(
                rates.into_iter().enumerate().map(|(index, rate)| {
                    workspace::compact_segment(
                        format!("sr-{rate}"),
                        if rate >= 1000 {
                            format!("{}k", rate / 1000)
                        } else {
                            rate.to_string()
                        },
                        self.resample_target == rate,
                        workspace::segment_position(index, count),
                        cx.listener(move |this, _, _, cx| {
                            this.resample_target = rate;
                            cx.notify();
                        }),
                    )
                }),
            )),
        )
    }

    fn channel_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let overlay =
            workspace::overlay_stack().child(workspace::overlay_line(self.channel.label()));
        workspace::stage(
            workspace::viz_frame(
                viz::channel_matrix_view(self.channel.to_tag() as usize),
                overlay,
            ),
            workspace::control_strip(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(space::HAIR))
                    .children(ChannelTransform::ALL.into_iter().map(|mode| {
                        workspace::latch(
                            format!("ch-{}", mode.to_tag()),
                            mode.label(),
                            self.channel == mode,
                            true,
                            cx.listener(move |this, _, _, cx| {
                                this.channel = mode;
                                this.emit_preview(cx);
                                cx.notify();
                            }),
                        )
                    })),
            ),
        )
    }

    fn phase_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let trail: Vec<(f32, f32)> = self.gonio_trail.iter().copied().collect();
        let history: Vec<f32> = self.corr_history.iter().copied().collect();
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(if self.phase_ms {
                "M/S"
            } else {
                "L/R"
            }))
            .child(workspace::overlay_line(format!(
                "Corr {:+.2}",
                self.phase.correlation
            )));
        workspace::stage(
            workspace::viz_frame(
                viz::goniometer_view(&trail, self.phase.correlation, &history),
                overlay,
            ),
            workspace::control_strip({
                let options = [(false, "L/R"), (true, "M/S")];
                let count = options.len();
                workspace::segment_track().children(options.into_iter().enumerate().map(
                    |(index, (ms, label))| {
                        workspace::compact_segment(
                            format!("phase-{label}"),
                            label,
                            self.phase_ms == ms,
                            workspace::segment_position(index, count),
                            cx.listener(move |this, _, _, cx| {
                                this.phase_ms = ms;
                                cx.notify();
                            }),
                        )
                    },
                ))
            }),
        )
    }

    fn dc_body(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(format!("L {:+.4}", self.dc.left)))
            .child(workspace::overlay_line(format!("R {:+.4}", self.dc.right)));
        workspace::stage(
            workspace::viz_frame(
                viz::dc_view(self.dc.left, self.dc.right, &self.envelope),
                overlay,
            ),
            workspace::control_strip(div().child(workspace::overlay_line(
                "Offset lines on the waveform mean. Apply removes DC.",
            ))),
        )
    }

    fn bpm_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let values: Vec<(f32, f32)> = self.bpm.iter().map(|c| (c.bpm, c.confidence)).collect();
        let overlay = workspace::overlay_stack().child(workspace::overlay_line(
            self.bpm
                .first()
                .map(|c| format!("{:.2} BPM  {:.0}%", c.bpm, c.confidence * 100.0))
                .unwrap_or_else(|| "No tempo yet".into()),
        ));
        workspace::stage(
            workspace::viz_frame(viz::candidate_bars(&values, 0), overlay),
            workspace::control_strip(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .child(workspace::param_row(
                        "Min",
                        workspace::unipolar_slider(
                            "bpm-min",
                            self.bpm_min,
                            40.0,
                            200.0,
                            bind_f32(cx, |this, value, cx| {
                                this.bpm_min = value.min(this.bpm_max - 1.0);
                                cx.notify();
                            }),
                            Some(60.0),
                        ),
                        format!("{:.0}", self.bpm_min),
                    ))
                    .child(workspace::param_row(
                        "Max",
                        workspace::unipolar_slider(
                            "bpm-max",
                            self.bpm_max,
                            60.0,
                            300.0,
                            bind_f32(cx, |this, value, cx| {
                                this.bpm_max = value.max(this.bpm_min + 1.0);
                                cx.notify();
                            }),
                            Some(200.0),
                        ),
                        format!("{:.0}", self.bpm_max),
                    ))
                    .child(
                        div()
                            .flex()
                            .gap(px(space::HAIR))
                            .child(inspector_mini_button(
                                "bpm-use",
                                "Use Original BPM",
                                self.bpm.first().is_some(),
                                cx.listener(|this, _, _, cx| {
                                    if let Some(best) = this.bpm.first() {
                                        this.dispatch(
                                            AudioToolCommand::UseOriginalBpm {
                                                clip_id: this.session.target.clip_id.clone(),
                                                bpm: best.bpm as f64,
                                            },
                                            cx,
                                        );
                                    }
                                }),
                            ))
                            .child(inspector_mini_button(
                                "bpm-tempo",
                                "Tempo Marker",
                                self.bpm.first().is_some(),
                                cx.listener(|this, _, _, cx| {
                                    if let Some(best) = this.bpm.first() {
                                        let beat = this
                                            .timeline
                                            .read(cx)
                                            .state
                                            .find_clip(&this.session.target.clip_id)
                                            .map(|(_, c)| c.start_beat as f64)
                                            .unwrap_or(0.0);
                                        this.dispatch(
                                            AudioToolCommand::AddTempoPoint {
                                                beat,
                                                bpm: best.bpm as f64,
                                            },
                                            cx,
                                        );
                                    }
                                }),
                            )),
                    ),
            ),
        )
    }

    fn key_body(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut conf = [0.0f32; 12];
        for key in &self.keys {
            let idx = tonic_index(key.tonic);
            conf[idx] = conf[idx].max(key.confidence);
        }
        let tonic = self
            .keys
            .first()
            .map(|k| tonic_index(k.tonic) as u8)
            .unwrap_or(0);
        let values: Vec<(f32, f32)> = self
            .keys
            .iter()
            .map(|k| (tonic_index(k.tonic) as f32, k.confidence))
            .collect();
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(
                self.keys
                    .first()
                    .map(|k| k.display_label())
                    .unwrap_or_else(|| "No key yet".into()),
            ))
            .child(workspace::overlay_line(
                self.keys
                    .first()
                    .map(|k| format!("Confidence {:.2}", k.confidence))
                    .unwrap_or_default(),
            ));
        workspace::stage(
            workspace::viz_frame(
                div()
                    .flex()
                    .size_full()
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .child(viz::pitch_class_view(tonic, &conf)),
                    )
                    .child(
                        div()
                            .w(px(120.0))
                            .h_full()
                            .child(viz::candidate_bars(&values, 0)),
                    ),
                overlay,
            ),
            workspace::control_strip(div().child(workspace::overlay_line(
                "Pitch-class energy · ranked candidates",
            ))),
        )
    }

    fn spectral_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mag = self
            .spectrum
            .as_ref()
            .map(|s| s.magnitudes_db.as_slice())
            .unwrap_or(&[]);
        let sr = self
            .spectrum
            .as_ref()
            .map(|s| s.sample_rate)
            .unwrap_or(self.session.target.sample_rate);
        let sel = self.session.target.spectral_selection;
        let min_hz = sel.map(|s| s.min_hz).unwrap_or(20.0);
        let max_hz = sel.map(|s| s.max_hz).unwrap_or(sr as f32 * 0.5);
        let overlay = workspace::overlay_stack()
            .child(workspace::overlay_line(format!(
                "{min_hz:.0}–{max_hz:.0} Hz"
            )))
            .child(workspace::overlay_line(format!(
                "Gain {:+.1} dB",
                self.spectral_gain_db
            )));
        workspace::stage(
            workspace::viz_frame(
                self.tracked_plot(
                    viz::spectral_gain_view(
                        mag,
                        sr,
                        min_hz,
                        max_hz,
                        self.spectral_gain_db,
                        self.graph_draw(),
                    ),
                    cx,
                ),
                overlay,
            ),
            workspace::control_strip(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .child(workspace::param_row(
                        "Gain",
                        workspace::unipolar_slider(
                            "sg-gain",
                            self.spectral_gain_db,
                            -48.0,
                            24.0,
                            bind_f32(cx, |this, value, cx| {
                                this.spectral_gain_db = value;
                                cx.notify();
                            }),
                            Some(0.0),
                        ),
                        format!("{:+.1} dB", self.spectral_gain_db),
                    ))
                    .child(inspector_mini_button(
                        "sg-silence",
                        "Silence Band",
                        true,
                        cx.listener(|this, _, _, cx| {
                            this.spectral_gain_db = -120.0;
                            cx.notify();
                        }),
                    ))
                    .child(self.graph_draw_tracks(cx)),
            ),
        )
    }

    fn spectrogram_settings_body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mag = self
            .spectrum
            .as_ref()
            .map(|s| s.magnitudes_db.as_slice())
            .unwrap_or(&[]);
        let hold = self
            .spectrum
            .as_ref()
            .map(|s| s.peak_hold_db.as_slice())
            .unwrap_or(&[]);
        let avg = self.spectrum_avg.as_deref().unwrap_or(&[]);
        let sr = self.session.target.sample_rate;
        workspace::stage(
            workspace::viz_frame(
                self.tracked_plot(
                    viz::spectrum_view(mag, avg, hold, sr, self.graph_draw()),
                    cx,
                ),
                workspace::overlay_line("FFT window for Spectrum Analyzer"),
            ),
            workspace::control_strip(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(space::TIGHT))
                    .child(self.fft_track(cx))
                    .child(self.window_track(cx))
                    .child(self.graph_draw_tracks(cx)),
            ),
        )
    }
}

pub fn open_audio_tool_window(
    kind: AudioToolKind,
    target: AudioToolTarget,
    owner_bounds: Option<Bounds<Pixels>>,
    remembered: Option<Bounds<Pixels>>,
    timeline: Entity<Timeline>,
    callbacks: AudioToolWindowCallbacks,
    cx: &mut App,
) -> Result<WindowHandle<AudioToolWindow>, String> {
    let (w, h) = kind.default_size();
    let window_size = size(px(w), px(h));
    let bounds =
        remembered.unwrap_or_else(|| centered_window_bounds(owner_bounds, window_size, cx));
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(bounds));
    options.kind = WindowKind::Normal;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(AUDIO_TOOL_WINDOW_MIN_WIDTH),
        px(AUDIO_TOOL_WINDOW_MIN_HEIGHT),
    ));
    apply_owner_display(&mut options, owner_bounds, cx);
    let session = AudioToolSession::new(kind, target);
    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| AudioToolWindow::new(session, timeline, callbacks, cx))
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::processed_output_path;
    use std::path::PathBuf;

    #[test]
    fn processed_path_is_unique_per_clip_and_stable_on_reapply() {
        assert_eq!(
            processed_output_path(PathBuf::from("/tmp/kick.wav").as_path(), "clip-3"),
            PathBuf::from("/tmp/kick.clip-3.processed.wav")
        );
        assert_eq!(
            processed_output_path(
                PathBuf::from("/tmp/kick.clip-3.processed.wav").as_path(),
                "clip-3"
            ),
            PathBuf::from("/tmp/kick.clip-3.processed.wav")
        );
        assert_ne!(
            processed_output_path(PathBuf::from("/tmp/kick.wav").as_path(), "clip-3"),
            processed_output_path(PathBuf::from("/tmp/kick.wav").as_path(), "clip-4")
        );
    }
}

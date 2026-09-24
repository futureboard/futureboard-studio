//! Neural Amp Modeler capture engine — NAM A2 first (SlimmableContainer and
//! A2 WaveNet), with A1 WaveNet/LSTM still loading so existing `.nam` files
//! keep working. Distinct from the classic modeled [`super::amp::Amp`],
//! selectable as an alternative engine for the same Tone/Amp slot (see
//! [`super::ToneEngineKind`]).
//!
//! Loading a `.nam` file parses JSON and builds a neural network — real
//! allocation, definitely not audio-thread work. [`prepare_nam_runtime`] does
//! that on the control thread and hands back a [`PreparedNamRuntime`] the
//! caller boxes and pushes into [`NamCapture::submit`]. The audio thread only
//! ever adopts it at a block boundary ([`NamCapture::begin_block`]); the
//! previous runtime is cross-faded out over a short window, then handed back
//! to the control thread ([`NamCapture::poll_garbage`]) to actually drop —
//! never inside [`NamCapture::process`].
//!
//! A SlimmableContainer (the usual NAM A2 file) holds every quality tier up
//! front. Switching the width dial ([`PreparedNamRuntime::set_slim_size`]) is
//! a single index write — real-time-safe, no rebuild.

use std::sync::Arc;

use builtin_dsp_core::make_eq_coefficients;
use nam_rs::{Model, NamModel};

use super::StereoBiquad;
use super::handoff::HandoffCell;
use super::rate::{MAX_RATIO, MIN_RATIO, RateAdapter};

/// Target integrated loudness (LUFS) captures are normalized to when loudness
/// normalization is enabled, matching the reference NAM plugin's convention.
const TARGET_LUFS: f32 = -18.0;

/// A `.nam` file's declared sample rate counts as matching the engine's within
/// this many Hz. Inside the tolerance the capture runs natively; outside it a
/// [`RateAdapter`] runs the model at its own rate (nam-rs does not resample,
/// and a mismatched model silently mis-runs its dilations/recurrence).
const SAMPLE_RATE_TOLERANCE_HZ: f64 = 0.5;

/// Roughly how long a runtime swap crossfades for, in milliseconds.
const SWAP_FADE_MS: f32 = 8.0;

/// Engine samples the capture buffers before running one inference, and
/// therefore the engine's exact latency while the NAM tone engine is selected.
///
/// nam-rs has two entry points that produce identical output: `process_sample`
/// and the block kernel `process_buffer`, which pushes a whole chunk through
/// one weight matrix at a time instead of reloading every matrix per sample.
/// Measured on a stock "standard" capture the block kernel is ~2.8x faster at
/// 64 samples (13.6 → 4.8 µs/sample) and ~3.5x at 128. 64 buys most of that
/// speedup for 1.3 ms at 48 kHz — the trade a per-sample host callback forces,
/// since the block cannot be computed until its last input sample has arrived.
pub const NAM_BLOCK: usize = 64;

/// A `.nam` failed to parse/build, or its sample rate is too far from the
/// engine's for the rate adapter to bridge.
#[derive(Debug)]
pub enum NamLoadError {
    Parse(nam_rs::Error),
    SampleRateMismatch { expected: f64, engine: f64 },
}

impl std::fmt::Display for NamLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamLoadError::Parse(e) => write!(f, "NAM capture failed to load: {e}"),
            NamLoadError::SampleRateMismatch { expected, engine } => write!(
                f,
                "NAM capture expects {expected} Hz but the engine runs at {engine} Hz — \
                 more than {MAX_RATIO}x apart, too far to adapt (supported: \
                 {MIN_RATIO}x..{MAX_RATIO}x the engine rate)"
            ),
        }
    }
}

impl std::error::Error for NamLoadError {}

/// Info handed back to the host/UI after a successful load — enough to show
/// the capture's name, warn about startup latency, and offer "Bypass Cab" when
/// the capture already models a full rig (amp + cab + mic). Serde-serializable
/// so it can travel over the plugin-host IPC as-is.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NamCaptureInfo {
    pub name: String,
    pub full_rig: bool,
    pub receptive_field: usize,
    pub sample_rate: f64,
    /// File-declared architecture (`WaveNet`, `LSTM`, `SlimmableContainer`, …).
    #[serde(default)]
    pub architecture: String,
    /// Coarse family the editor badges: `a2`, `a1`, or `lstm`.
    #[serde(default)]
    pub family: String,
    /// True when the file is a NAM A2 SlimmableContainer (runtime quality dial).
    #[serde(default)]
    pub slimmable: bool,
    /// How many pre-built submodels a slimmable capture holds (0 otherwise).
    #[serde(default)]
    pub submodel_count: usize,
}

/// A fully-built, ready-to-run capture. Boxed and moved into [`NamCapture`]'s
/// hand-off cell; built entirely on the control thread by
/// [`prepare_nam_runtime`].
pub struct PreparedNamRuntime {
    name: String,
    architecture: String,
    family: String,
    slimmable: bool,
    submodel_count: usize,
    model_l: Model,
    /// `None` for a mono capture: the single model's output is mirrored to
    /// both channels rather than running two redundant inferences.
    model_r: Option<Model>,
    sample_rate: f64,
    receptive_field: usize,
    /// Precomputed linear gain to bring the capture to [`TARGET_LUFS`], or
    /// `1.0` if the file carries no loudness metadata. Computed once here so
    /// the hot path is a single multiply, not a per-sample dB calculation.
    loudness_gain: f32,
    full_rig: bool,
    /// Present only when the capture's rate differs from the engine's — it
    /// runs the model at the capture's own rate from inside the engine's
    /// stream. `None` is the native, zero-overhead path.
    adapter: Option<RateAdapter>,
    /// The capture's own latency in **engine** samples, on top of the
    /// [`NAM_BLOCK`] the buffering costs: zero for a natively-run capture,
    /// and the adapter's interpolation delay for a rate-adapted one.
    ///
    /// The receptive field is deliberately *not* part of this. A WaveNet's
    /// dilated convolutions are causal — output sample `n` is computed from
    /// inputs up to `n` — so the field is how much history the model needs
    /// warmed, not a delay it imposes. [`prepare_nam_runtime`] warms it here,
    /// on the control thread, exactly like the reference NAM plugin.
    latency_samples: usize,
    /// Inference staging, `NAM_BLOCK` long: the model runs mono, in place, on
    /// one channel at a time.
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
}

impl std::fmt::Debug for PreparedNamRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedNamRuntime")
            .field("name", &self.name)
            .field("sample_rate", &self.sample_rate)
            .field("receptive_field", &self.receptive_field)
            .field("full_rig", &self.full_rig)
            .field("architecture", &self.architecture)
            .field("family", &self.family)
            .field("slimmable", &self.slimmable)
            .field("resampled", &self.adapter.is_some())
            .finish_non_exhaustive()
    }
}

/// Parse and build a `.nam` capture off the audio thread. `stereo` selects
/// whether a second, independent model is built for the right channel (true
/// stereo width) or the left model's output is mirrored (mono bus / cheaper).
///
/// A capture whose declared rate differs from the engine's is *not* rejected:
/// it gets a [`RateAdapter`] that runs the model at its own rate from inside
/// the engine's stream, so a 44.1 kHz capture is usable in a 48 kHz session.
/// Only a ratio outside the adapter's supported range is refused — see
/// [`NamLoadError::SampleRateMismatch`]. Running the session at the capture's
/// own rate still sounds better; the adapter is a convenience, not a wash.
pub fn prepare_nam_runtime(
    json: &str,
    name: String,
    engine_sample_rate: f64,
    stereo: bool,
    full_rig: bool,
) -> Result<PreparedNamRuntime, NamLoadError> {
    let nam_model = NamModel::from_json_str(json).map_err(NamLoadError::Parse)?;
    let expected = nam_model.expected_sample_rate();
    let adapter = if (expected - engine_sample_rate).abs() > SAMPLE_RATE_TOLERANCE_HZ {
        Some(
            RateAdapter::new(expected, engine_sample_rate, NAM_BLOCK).ok_or(
                NamLoadError::SampleRateMismatch {
                    expected,
                    engine: engine_sample_rate,
                },
            )?,
        )
    } else {
        None
    };

    let mut model_l = Model::from_nam(&nam_model).map_err(NamLoadError::Parse)?;
    let mut model_r = if stereo {
        Some(Model::from_nam(&nam_model).map_err(NamLoadError::Parse)?)
    } else {
        None
    };

    // A2 SlimmableContainer: warm every submodel, then park on the full-width
    // one. Switching later is a single index write and must not allocate.
    prewarm_model(&mut model_l);
    if let Some(model_r) = model_r.as_mut() {
        prewarm_model(model_r);
    }

    let slimmable = model_l.as_slimmable().is_some();
    let submodel_count = model_l.as_slimmable().map(|s| s.len()).unwrap_or(0);
    let family = nam_family(&nam_model, slimmable).to_string();
    let architecture = nam_model.architecture.clone();
    let receptive_field = model_l.receptive_field();
    let loudness_gain = nam_model
        .loudness()
        .map(|l| 10f32.powf((TARGET_LUFS - l) / 20.0).clamp(0.05, 20.0))
        .unwrap_or(1.0);
    let full_rig = full_rig || nam_model.includes_cab() == Some(true);
    let name = {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            nam_model
                .metadata_typed()
                .name
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| "NAM Capture".into())
        } else {
            trimmed.to_string()
        }
    };

    let latency_samples = adapter.as_ref().map_or(0, RateAdapter::latency_samples);

    Ok(PreparedNamRuntime {
        name,
        architecture,
        family,
        slimmable,
        submodel_count,
        model_l,
        model_r,
        sample_rate: expected,
        receptive_field,
        loudness_gain,
        full_rig,
        adapter,
        latency_samples,
        scratch_l: vec![0.0; NAM_BLOCK],
        scratch_r: vec![0.0; NAM_BLOCK],
    })
}

fn nam_family(nam: &NamModel, slimmable: bool) -> &'static str {
    let arch = nam.architecture.to_ascii_lowercase();
    if slimmable || arch.contains("slimmable") || arch.contains("a2") {
        "a2"
    } else if arch.contains("lstm") {
        "lstm"
    } else {
        "a1"
    }
}

/// Warm a freshly built model (and every slimmable submodel) against silence
/// so the first real block is not a startup transient. Control thread only.
fn prewarm_model(model: &mut Model) {
    let submodels: Vec<usize> = model
        .as_slimmable()
        .map(|slim| (0..slim.len()).collect())
        .unwrap_or_default();
    if submodels.is_empty() {
        let samples = model.receptive_field();
        prewarm(model, samples);
        return;
    }
    for index in submodels {
        if let Some(slim) = model.as_slimmable_mut() {
            slim.select(index);
        }
        let samples = model.receptive_field();
        prewarm(model, samples);
    }
    if let Some(slim) = model.as_slimmable_mut() {
        let last = slim.len().saturating_sub(1);
        slim.select(last);
    }
}

/// Run `samples` of silence through a freshly built model so its dilation
/// history holds the model's own steady-state response to silence instead of
/// hard zeros. Control thread only: this is the expensive part of a load.
fn prewarm(model: &mut Model, samples: usize) {
    const CHUNK: usize = 256;
    let mut silence = [0.0f32; CHUNK];
    let mut done = 0;
    while done < samples {
        let n = CHUNK.min(samples - done);
        silence[..n].fill(0.0);
        model.process_buffer(&mut silence[..n]);
        done += n;
    }
}

impl PreparedNamRuntime {
    pub fn info(&self) -> NamCaptureInfo {
        NamCaptureInfo {
            name: self.name.clone(),
            full_rig: self.full_rig,
            receptive_field: self.receptive_field,
            sample_rate: self.sample_rate,
            architecture: self.architecture.clone(),
            family: self.family.clone(),
            slimmable: self.slimmable,
            submodel_count: self.submodel_count,
        }
    }

    /// Audio-thread-safe width dial for a SlimmableContainer. No-op on a
    /// single WaveNet/LSTM. `size` is 0..1 (0 = smallest submodel, 1 = full).
    pub fn set_slim_size(&mut self, size: f32) {
        let size = size.clamp(0.0, 1.0);
        if let Some(slim) = self.model_l.as_slimmable_mut() {
            slim.set_slim_size(size);
        }
        if let Some(model_r) = self.model_r.as_mut() {
            if let Some(slim) = model_r.as_slimmable_mut() {
                slim.set_slim_size(size);
            }
        }
    }

    /// Audio thread: run one block of at most [`NAM_BLOCK`] engine samples.
    /// `out_*` receive the model's output; nothing here allocates or locks.
    fn process_block(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        loudness_on: bool,
    ) {
        // Split the borrow so the inference closure and the adapter can be
        // held mutably at the same time.
        let Self {
            model_l,
            model_r,
            adapter,
            loudness_gain,
            scratch_l,
            scratch_r,
            ..
        } = self;
        let gain = if loudness_on { *loudness_gain } else { 1.0 };
        // One call per weight matrix per block instead of per sample — this is
        // the whole reason the capture is buffered.
        let mut infer = |l: &mut [f32], r: &mut [f32]| {
            model_l.process_buffer(l);
            match model_r.as_mut() {
                Some(model_r) => {
                    model_r.process_buffer(r);
                    for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                        *a *= gain;
                        *b *= gain;
                    }
                }
                // Mono capture: one inference, mirrored to both channels.
                None => {
                    for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                        *a *= gain;
                        *b = *a;
                    }
                }
            }
        };

        match adapter.as_mut() {
            // Rate-adapted: the model still sees its own rate, and still runs
            // over a whole staged chunk rather than sample by sample.
            Some(adapter) => adapter.run_block(in_l, in_r, out_l, out_r, infer),
            None => {
                let n = in_l.len();
                scratch_l[..n].copy_from_slice(in_l);
                scratch_r[..n].copy_from_slice(in_r);
                infer(&mut scratch_l[..n], &mut scratch_r[..n]);
                out_l[..n].copy_from_slice(&scratch_l[..n]);
                out_r[..n].copy_from_slice(&scratch_r[..n]);
            }
        }
    }

    /// Clear the streaming state a transport stop should clear — but
    /// deliberately **not** the model's dilation history.
    ///
    /// That history was warmed at load ([`prewarm`]) precisely so the capture
    /// has no startup transient; zeroing it here would bring the transient
    /// back on every stop, and the host is no longer delaying the track by the
    /// receptive field to hide it. A stopped transport feeds the model silence
    /// anyway, which is the state the prewarm put it in.
    fn reset(&mut self) {
        if let Some(adapter) = self.adapter.as_mut() {
            adapter.reset();
        }
    }
}

/// The two lock-free hand-off cells between the control side and the audio
/// side, shareable via [`NamLoader`] so a control thread in another ownership
/// domain (e.g. the plugin-host IPC thread, which must never touch the `Dsp`
/// the audio producer owns) can still submit captures and drain garbage.
pub struct NamChannel {
    /// Control thread → audio thread: a freshly built runtime awaiting adoption.
    pending: HandoffCell<PreparedNamRuntime>,
    /// Audio thread → control thread: a retired runtime awaiting disposal.
    retired: HandoffCell<PreparedNamRuntime>,
}

/// Cloneable control-side handle to a [`NamCapture`]'s hand-off cells.
///
/// Thread contract: exactly **one** control thread may use the loader at a
/// time (the cells are single-producer/single-consumer per direction) — the
/// same discipline `Dsp::load_nam_capture_json` always required, now
/// structurally separated from `&mut Dsp` so it works across the
/// `UnsafeCell` ownership boundary in the plugin-host process.
#[derive(Clone)]
pub struct NamLoader {
    channel: Arc<NamChannel>,
}

impl NamLoader {
    /// Push a freshly-built runtime for the audio thread to adopt at the next
    /// block boundary. A not-yet-adopted runtime already waiting is dropped
    /// here (safe: the audio thread never touched it).
    pub fn submit(&self, runtime: Box<PreparedNamRuntime>) {
        if let Some(bumped) = self.channel.pending.put(runtime) {
            drop(bumped);
        }
    }

    /// Drop any runtime the audio thread has retired. Call periodically and
    /// before each [`Self::submit`] as an opportunistic sweep.
    pub fn collect_garbage(&self) {
        if let Some(dead) = self.channel.retired.take() {
            drop(dead);
        }
    }

    /// Parse, build and submit a `.nam` capture in one call (control thread —
    /// parsing allocates and can take a while for large models).
    pub fn load_json(
        &self,
        json: &str,
        name: impl Into<String>,
        engine_sample_rate: f64,
        stereo: bool,
        full_rig: bool,
    ) -> Result<NamCaptureInfo, NamLoadError> {
        let prepared =
            prepare_nam_runtime(json, name.into(), engine_sample_rate, stereo, full_rig)?;
        let info = prepared.info();
        self.collect_garbage();
        self.submit(Box::new(prepared));
        Ok(info)
    }
}

/// The audio-thread-resident NAM engine: a preallocated DC blocker, live trim/
/// mix/loudness knobs, and the lock-free hand-off machinery that lets the
/// control thread swap in a freshly-built [`PreparedNamRuntime`] without ever
/// blocking or allocating on the audio thread.
pub(super) struct NamCapture {
    active: Option<Box<PreparedNamRuntime>>,
    /// The just-replaced runtime, still running so [`Self::process`] can
    /// crossfade away from it instead of cutting over with a click.
    fading_out: Option<Box<PreparedNamRuntime>>,
    /// A retiree that didn't fit in `retired` (control thread hasn't drained
    /// it yet). Held here — never dropped on the audio thread — until a later
    /// block finds `retired` empty again.
    retire_overflow: Option<Box<PreparedNamRuntime>>,

    /// The shared hand-off cells; [`Self::loader`] clones this out.
    channel: Arc<NamChannel>,

    /// Input ring: the block being filled. Doubles as the dry-signal delay
    /// line, so the wet/dry mix stays phase-aligned instead of comb-filtering
    /// the block delay against itself.
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    /// The active runtime's output for the previous block.
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    /// The retiring runtime's output for the same block, crossfaded against
    /// `out_*` while a swap resolves.
    fade_l: Vec<f32>,
    fade_r: Vec<f32>,
    /// Input staging with the input trim applied — the model must see the
    /// trimmed signal while `in_*` keeps the untrimmed dry.
    trim_l: Vec<f32>,
    trim_r: Vec<f32>,
    /// How many samples of the current block have been written.
    fill: usize,

    sample_rate: f32,
    dc_hpf: StereoBiquad,

    input_trim: f32,
    output_trim: f32,
    loudness_norm_on: bool,
    mix: f32,
    /// 0..1 width dial for a SlimmableContainer. Ignored by single models.
    slim_size: f32,

    /// 0 = fully `fading_out`, 1 = fully `active`. Sits at 1.0 when no fade
    /// is in progress.
    fade: f32,
    fade_step: f32,
}

impl NamCapture {
    pub(super) fn new(sample_rate: f32) -> Self {
        let mut me = Self {
            active: None,
            fading_out: None,
            retire_overflow: None,
            channel: Arc::new(NamChannel {
                pending: HandoffCell::new(),
                retired: HandoffCell::new(),
            }),
            in_l: vec![0.0; NAM_BLOCK],
            in_r: vec![0.0; NAM_BLOCK],
            out_l: vec![0.0; NAM_BLOCK],
            out_r: vec![0.0; NAM_BLOCK],
            fade_l: vec![0.0; NAM_BLOCK],
            fade_r: vec![0.0; NAM_BLOCK],
            trim_l: vec![0.0; NAM_BLOCK],
            trim_r: vec![0.0; NAM_BLOCK],
            fill: 0,
            sample_rate: sample_rate.max(1.0),
            dc_hpf: StereoBiquad::none(),
            input_trim: 1.0,
            output_trim: 1.0,
            loudness_norm_on: true,
            mix: 1.0,
            slim_size: 1.0,
            fade: 1.0,
            fade_step: 1.0,
        };
        me.recompute_sample_rate_derived();
        me
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.recompute_sample_rate_derived();
    }

    fn recompute_sample_rate_derived(&mut self) {
        self.dc_hpf.set(make_eq_coefficients(
            "highpass",
            20.0,
            0.0,
            0.707,
            self.sample_rate,
        ));
        let fade_len = (self.sample_rate * (SWAP_FADE_MS / 1_000.0)).max(1.0);
        self.fade_step = 1.0 / fade_len;
    }

    pub(super) fn reset(&mut self) {
        self.dc_hpf.reset();
        for buffer in [
            &mut self.in_l,
            &mut self.in_r,
            &mut self.out_l,
            &mut self.out_r,
            &mut self.fade_l,
            &mut self.fade_r,
        ] {
            buffer.fill(0.0);
        }
        self.fill = 0;
        if let Some(rt) = self.active.as_mut() {
            rt.reset();
        }
        if let Some(rt) = self.fading_out.as_mut() {
            rt.reset();
        }
    }

    /// Live knob update (control thread only): trims in dB, mix in 0..100 %,
    /// slim size in 0..1 (NAM A2 quality dial).
    pub(super) fn configure(
        &mut self,
        input_trim_db: f32,
        output_trim_db: f32,
        mix_pct: f32,
        loudness_norm_on: bool,
        slim_size: f32,
    ) {
        self.input_trim = db_to_linear(input_trim_db);
        self.output_trim = db_to_linear(output_trim_db);
        self.mix = (mix_pct / 100.0).clamp(0.0, 1.0);
        self.loudness_norm_on = loudness_norm_on;
        self.slim_size = slim_size.clamp(0.0, 1.0);
        self.apply_slim_size();
    }

    fn apply_slim_size(&mut self) {
        let size = self.slim_size;
        if let Some(rt) = self.active.as_mut() {
            rt.set_slim_size(size);
        }
    }

    /// Clone out a control-side loader handle for this capture's cells.
    pub(super) fn loader(&self) -> NamLoader {
        NamLoader {
            channel: Arc::clone(&self.channel),
        }
    }

    /// Control thread: push a freshly-built runtime for the audio thread to
    /// adopt at the next block boundary. Any not-yet-adopted runtime already
    /// waiting is dropped here (safe: the audio thread never touched it).
    pub(super) fn submit(&self, runtime: Box<PreparedNamRuntime>) {
        if let Some(bumped) = self.channel.pending.put(runtime) {
            drop(bumped);
        }
    }

    /// Control thread: drop any runtime the audio thread has retired. Call
    /// periodically (e.g. an idle/UI timer); also safe to call before
    /// [`Self::submit`] as an opportunistic sweep.
    pub(super) fn poll_garbage(&mut self) {
        if let Some(dead) = self.channel.retired.take() {
            drop(dead);
        }
    }

    /// Info about the currently active capture, if one is loaded.
    pub(super) fn active_info(&self) -> Option<NamCaptureInfo> {
        self.active.as_ref().map(|rt| rt.info())
    }

    /// Latency this engine adds, in **engine** samples: the [`NAM_BLOCK`] the
    /// inference buffering costs, plus a rate-adapted capture's interpolation
    /// delay. The receptive field is not part of it — see
    /// [`PreparedNamRuntime::latency_samples`].
    ///
    /// It is reported whether or not a capture is loaded, because the block
    /// buffer runs either way: an empty slot that reported 0 would force the
    /// host to redo delay compensation the moment a capture landed, mid-play.
    pub(super) fn latency_samples(&self) -> usize {
        NAM_BLOCK
            + self
                .active
                .as_ref()
                .map(|rt| rt.latency_samples)
                .unwrap_or(0)
    }

    /// Audio thread: adopt a pending runtime and retire a finished fade.
    /// Called once per audio block, never per sample — this is the only place
    /// the swap happens.
    pub(super) fn begin_block(&mut self) {
        // Drain a previous overflow now that a block boundary has passed.
        if let Some(carry) = self.retire_overflow.take() {
            if let Some(bounced) = self.channel.retired.put(carry) {
                self.retire_overflow = Some(bounced);
            }
        }

        // Only start a new swap once any in-progress fade has fully resolved,
        // so at most one runtime is ever "in flight" toward retirement.
        if self.fading_out.is_none() {
            if let Some(new_rt) = self.channel.pending.take() {
                if let Some(old) = self.active.replace(new_rt) {
                    self.fading_out = Some(old);
                    self.fade = 0.0;
                    // The current block was already computed by the outgoing
                    // runtime. Seed the fade buffer with it so the crossfade
                    // blends continuous audio for the rest of this block
                    // instead of whatever an earlier swap left behind.
                    self.fade_l.copy_from_slice(&self.out_l);
                    self.fade_r.copy_from_slice(&self.out_r);
                }
                self.apply_slim_size();
            }
        }

        if self.fade >= 1.0 {
            if let Some(done) = self.fading_out.take() {
                if let Some(bounced) = self.channel.retired.put(done) {
                    self.retire_overflow = Some(bounced);
                }
            }
        }
    }

    /// Audio thread hot path. One sample in, one sample out, delayed by
    /// [`NAM_BLOCK`]; the inference itself runs once every [`NAM_BLOCK`] calls
    /// over the whole block. Per sample this is only the DC blocker, the trims
    /// and the wet/dry mix — the dry side comes out of the same ring as the
    /// wet, so the two stay phase-aligned. No allocation, no locks, no swap
    /// logic (see [`Self::begin_block`]).
    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        // Read this slot's finished output and the dry sample that produced it
        // *before* the slot is overwritten with the newest input.
        let dry = (self.in_l[self.fill], self.in_r[self.fill]);
        let new_out = (self.out_l[self.fill], self.out_r[self.fill]);

        let wet = if self.fading_out.is_some() {
            let old_out = (self.fade_l[self.fill], self.fade_r[self.fill]);
            self.fade = (self.fade + self.fade_step).min(1.0);
            (
                old_out.0 * (1.0 - self.fade) + new_out.0 * self.fade,
                old_out.1 * (1.0 - self.fade) + new_out.1 * self.fade,
            )
        } else {
            new_out
        };

        self.in_l[self.fill] = left;
        self.in_r[self.fill] = right;
        self.fill += 1;
        if self.fill >= NAM_BLOCK {
            self.fill = 0;
            self.run_block();
        }

        let (mut ol, mut or) = self.dc_hpf.run(wet.0, wet.1);
        ol *= self.output_trim;
        or *= self.output_trim;

        let m = self.mix;
        (dry.0 * (1.0 - m) + ol * m, dry.1 * (1.0 - m) + or * m)
    }

    /// Run the just-filled input block through the active runtime (and the
    /// retiring one, while a swap crossfades). Called from [`Self::process`]
    /// every [`NAM_BLOCK`] samples; allocates nothing.
    fn run_block(&mut self) {
        let trim = self.input_trim;
        for i in 0..NAM_BLOCK {
            self.trim_l[i] = self.in_l[i] * trim;
            self.trim_r[i] = self.in_r[i] * trim;
        }

        match self.active.as_mut() {
            Some(rt) => rt.process_block(
                &self.trim_l,
                &self.trim_r,
                &mut self.out_l,
                &mut self.out_r,
                self.loudness_norm_on,
            ),
            None => {
                self.out_l.copy_from_slice(&self.trim_l);
                self.out_r.copy_from_slice(&self.trim_r);
            }
        }

        if let Some(old_rt) = self.fading_out.as_mut() {
            old_rt.process_block(
                &self.trim_l,
                &self.trim_r,
                &mut self.fade_l,
                &mut self.fade_r,
                self.loudness_norm_on,
            );
        }
    }
}

#[inline]
fn db_to_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_WAVENET_48K: &str = r#"{
        "version": "0.5.4", "architecture": "WaveNet",
        "config": { "layers": [{
            "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
            "kernel_size": 1, "dilations": [1], "activation": "ReLU",
            "gated": false, "head_bias": false
        }], "head": null, "head_scale": 1.0 },
        "weights": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0],
        "sample_rate": 48000.0
    }"#;

    /// A rate mismatch inside the adapter's range builds a runtime that runs
    /// the model at its own rate; only the adapter's own interpolation delay
    /// is latency, and it is counted in engine samples.
    #[test]
    fn adapts_a_mismatched_rate_instead_of_rejecting_it() {
        let native = prepare_nam_runtime(TINY_WAVENET_48K, "t".into(), 48_000.0, false, false)
            .expect("native rate loads");
        assert!(native.adapter.is_none(), "matching rate must not resample");
        assert_eq!(
            native.latency_samples, 0,
            "a natively-run capture is causal: no latency beyond the block"
        );

        let adapted = prepare_nam_runtime(TINY_WAVENET_48K, "t".into(), 44_100.0, false, false)
            .expect("a 48 kHz capture must adapt into a 44.1 kHz engine");
        assert!(adapted.adapter.is_some(), "mismatch must build an adapter");
        assert_eq!(
            adapted.sample_rate, 48_000.0,
            "info keeps the capture's rate"
        );
        assert!(
            adapted.latency_samples > 0 && adapted.latency_samples < NAM_BLOCK,
            "only the adapter's interpolation delay counts as latency, got {}",
            adapted.latency_samples
        );

        let mut cap = NamCapture::new(44_100.0);
        cap.configure(0.0, 0.0, 100.0, false, 1.0);
        cap.submit(Box::new(adapted));
        cap.begin_block();
        for n in 0..4_000 {
            let x = (n as f32 * 0.02).sin() * 0.3;
            let (l, r) = cap.process(x, -x);
            assert!(l.is_finite() && r.is_finite(), "resampled capture blew up");
        }
        assert!(cap.latency_samples() > 0 || cap.active_info().is_some());
    }

    #[test]
    fn refuses_a_rate_ratio_beyond_the_adapter_range() {
        // 48 kHz capture in a 6 kHz engine — an 8x ratio.
        let err = prepare_nam_runtime(TINY_WAVENET_48K, "t".into(), 6_000.0, false, false)
            .expect_err("an 8x rate ratio must be refused");
        assert!(matches!(err, NamLoadError::SampleRateMismatch { .. }));
    }

    #[test]
    fn loads_and_processes_at_matching_rate() {
        let prepared = prepare_nam_runtime(TINY_WAVENET_48K, "t".into(), 48_000.0, false, false)
            .expect("matching rate must load");
        assert_eq!(prepared.model_r.is_some(), false);
        assert_eq!(prepared.family, "a1");
        assert!(!prepared.slimmable);

        let mut cap = NamCapture::new(48_000.0);
        cap.configure(0.0, 0.0, 100.0, false, 1.0);
        cap.submit(Box::new(prepared));
        cap.begin_block();
        for _ in 0..64 {
            let (l, r) = cap.process(0.1, -0.1);
            assert!(l.is_finite() && r.is_finite());
        }
        assert!(cap.active_info().is_some());
    }

    #[test]
    fn stereo_capture_builds_two_independent_models() {
        let prepared = prepare_nam_runtime(TINY_WAVENET_48K, "t".into(), 48_000.0, true, true)
            .expect("matching rate must load");
        assert!(prepared.model_r.is_some());
        assert!(prepared.full_rig);
    }

    #[test]
    fn swap_crossfades_without_dropping_on_audio_thread() {
        let mut cap = NamCapture::new(48_000.0);
        cap.configure(0.0, 0.0, 100.0, false, 1.0);

        let first =
            prepare_nam_runtime(TINY_WAVENET_48K, "a".into(), 48_000.0, false, false).unwrap();
        cap.submit(Box::new(first));
        cap.begin_block();
        for _ in 0..8 {
            cap.process(0.2, 0.2);
        }

        let second =
            prepare_nam_runtime(TINY_WAVENET_48K, "b".into(), 48_000.0, false, false).unwrap();
        cap.submit(Box::new(second));
        cap.begin_block(); // adopts `second`, starts fading `first` out
        assert!(cap.fading_out.is_some());

        // Run well past the fade window; every sample must stay finite, and
        // the fade must fully resolve without the audio thread ever calling
        // `retired.take()` (only `begin_block` — called here — does).
        for _ in 0..2_000 {
            let (l, r) = cap.process(0.2, -0.2);
            assert!(l.is_finite() && r.is_finite());
            cap.begin_block();
        }
        assert!(cap.fading_out.is_none(), "fade must resolve and retire");

        // Control thread drains what the audio thread retired.
        cap.poll_garbage();
    }

    /// The loader handle must reach the same cells as the capture itself:
    /// submit through the loader, adopt via `begin_block`, retire, and drain
    /// garbage through the loader — the cross-process (host IPC thread) flow.
    #[test]
    fn loader_handle_round_trips_submit_and_garbage() {
        let mut cap = NamCapture::new(48_000.0);
        cap.configure(0.0, 0.0, 100.0, false, 1.0);
        let loader = cap.loader();

        let info = loader
            .load_json(TINY_WAVENET_48K, "via-loader", 48_000.0, false, false)
            .expect("load through loader");
        assert_eq!(info.name, "via-loader");

        cap.begin_block();
        assert_eq!(cap.active_info().map(|i| i.name), Some("via-loader".into()));

        // Swap in a second capture and let the fade retire the first.
        loader
            .load_json(TINY_WAVENET_48K, "second", 48_000.0, false, false)
            .expect("second load");
        cap.begin_block();
        for _ in 0..2_000 {
            let (l, r) = cap.process(0.2, -0.2);
            assert!(l.is_finite() && r.is_finite());
            cap.begin_block();
        }
        assert!(cap.fading_out.is_none());
        loader.collect_garbage(); // drains the retired first capture
        assert_eq!(cap.active_info().map(|i| i.name), Some("second".into()));
    }

    /// With no capture loaded the slot passes the signal through — delayed by
    /// the block the engine always buffers, which is exactly what
    /// `latency_samples()` reports so the host can compensate it.
    #[test]
    fn no_capture_loaded_is_a_delayed_pass_through_at_unity() {
        let mut cap = NamCapture::new(48_000.0);
        cap.configure(0.0, 0.0, 100.0, false, 1.0);
        assert_eq!(cap.latency_samples(), NAM_BLOCK);

        // A single pulse, so the delay can be read off directly rather than
        // through the DC blocker's (tiny, but non-zero) phase shift.
        let input: Vec<f32> = (0..NAM_BLOCK * 8)
            .map(|n| if n == 7 { 1.0 } else { 0.0 })
            .collect();
        let out: Vec<(f32, f32)> = input.iter().map(|&x| cap.process(x, -x)).collect();

        let peak_at = |pick: fn(&(f32, f32)) -> f32| {
            out.iter()
                .enumerate()
                .max_by(|a, b| pick(a.1).abs().total_cmp(&pick(b.1).abs()))
                .map(|(i, _)| i)
                .unwrap()
        };
        assert_eq!(
            peak_at(|s| s.0),
            7 + NAM_BLOCK,
            "left delay is not the block"
        );
        assert_eq!(
            peak_at(|s| s.1),
            7 + NAM_BLOCK,
            "right delay is not the block"
        );
        let energy: f32 = out.iter().map(|(l, _)| l * l).sum();
        assert!(
            (energy - 1.0).abs() < 0.05,
            "the dry path changed level: pulse energy {energy}"
        );
    }

    /// The wet/dry mix must blend the *aligned* dry sample. If the dry side
    /// skipped the block delay, a mix would comb-filter the signal against a
    /// 64-sample-old copy of itself — at 375 Hz that delay is half a period,
    /// so a misaligned 50 % mix cancels almost completely.
    #[test]
    fn the_wet_dry_mix_stays_phase_aligned_through_the_block_delay() {
        let mut cap = NamCapture::new(48_000.0);
        cap.configure(0.0, 0.0, 50.0, false, 1.0);

        let half_period = std::f32::consts::PI / NAM_BLOCK as f32;
        let input: Vec<f32> = (0..NAM_BLOCK * 32)
            .map(|n| (n as f32 * half_period).sin() * 0.5)
            .collect();
        let out: Vec<f32> = input.iter().map(|&x| cap.process(x, x).0).collect();

        let rms = |signal: &[f32]| {
            (signal.iter().map(|x| x * x).sum::<f32>() / signal.len() as f32).sqrt()
        };
        let settled = &out[NAM_BLOCK * 4..];
        let reference = rms(&input[NAM_BLOCK * 4..]);
        assert!(
            (rms(settled) / reference - 1.0).abs() < 0.02,
            "mix comb-filtered: {} vs {reference}",
            rms(settled)
        );
    }

    /// A freshly loaded capture must be warm: the reference plugin runs the
    /// receptive field through with silence at load rather than making the
    /// host delay the track by it.
    #[test]
    fn a_prepared_runtime_reports_no_receptive_field_latency() {
        let prepared = prepare_nam_runtime(TINY_WAVENET_48K, "t".into(), 48_000.0, false, false)
            .expect("matching rate must load");
        assert!(prepared.receptive_field >= 1, "the field is still reported");
        assert_eq!(prepared.latency_samples, 0);

        let mut cap = NamCapture::new(48_000.0);
        cap.configure(0.0, 0.0, 100.0, false, 1.0);
        cap.submit(Box::new(prepared));
        cap.begin_block();
        assert_eq!(
            cap.latency_samples(),
            NAM_BLOCK,
            "the block buffer is the only latency a native-rate capture adds"
        );
    }
}

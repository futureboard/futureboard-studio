//! Background loading of Solfege models, with progress that measures the work
//! actually being done.
//!
//! Opening a packaged model is not a quick metadata peek. `SoloViolin.sfm` is
//! 146 MB, almost all of it the `AUDO` voicebank section, and verifying it
//! hashes every one of those bytes twice — once for the file trailer, once for
//! the section's own digest. That cannot happen inside a render pass, and it
//! cannot be reported as a stage counter either: a bar that steps once per
//! stage would sit still through the entire hash.
//!
//! So the unit of progress here is **one byte moved or hashed**, and the
//! weights come from the SFM section table rather than from constants. A
//! physical-only package spends nothing on the voicebank stage and its bar
//! keeps moving; the sampled violin spends ~99% of its budget in `Opening` and
//! `Validating`, which is exactly where its seconds go.
//!
//! Results are cached per file *stamp* — path, length and mtime — so replacing
//! a model on disk re-reads it and a failure caused by a half-copied file does
//! not become permanent for the process lifetime.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use gpui::App;

use super::SolfegeModelInfo;

/// Bytes moved between two cancellation checkpoints while reading a model.
/// Matches [`solfege_model::VERIFY_CHUNK_BYTES`] so read and verify progress
/// land at the same granularity.
const READ_CHUNK_BYTES: usize = solfege_model::VERIFY_CHUNK_BYTES;

/// How often the panel repaints while a load is in flight. Progress is
/// published from a worker thread, which changes no GPUI entity, so the panel
/// has to poll — but at display rate it would repaint the whole shell hundreds
/// of times for one load. 80 ms keeps the percentage visibly live for a few
/// pennies of layout.
const PROGRESS_POLL: Duration = Duration::from_millis(80);

/// The step a model load has reached.
///
/// Every variant is a step this loader really performs; none is a placeholder.
/// `PreparingEngine` covers assembling the verified sections into the
/// [`SolfegeModelInfo`] the inspector and the engine adapter both read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelLoadStage {
    Opening,
    Validating,
    LoadingVoicebank,
    LoadingPhysicalModel,
    LoadingNeuralModel,
    PreparingEngine,
    Ready,
}

impl ModelLoadStage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Opening => "Opening",
            Self::Validating => "Validating",
            Self::LoadingVoicebank => "Loading voicebank",
            Self::LoadingPhysicalModel => "Loading physical model",
            Self::LoadingNeuralModel => "Loading neural model",
            Self::PreparingEngine => "Preparing engine",
            Self::Ready => "Ready",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelLoadProgress {
    pub stage: ModelLoadStage,
    /// 0.0 ..= 1.0 across the whole load, not within the stage.
    pub fraction: f32,
}

impl ModelLoadProgress {
    pub fn percent(self) -> f32 {
        self.fraction * 100.0
    }
}

/// A load failure that can name the step and the cause.
///
/// Reads as `Solo Violin failed to load: AUDO section checksum mismatch.` —
/// the model, the stage's own error, and enough of the format's vocabulary to
/// tell a truncated copy from a corrupt one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelLoadError {
    pub model: String,
    pub stage: ModelLoadStage,
    pub cause: String,
}

impl std::fmt::Display for ModelLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed to load: {}.", self.model, self.cause)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelLoadState {
    Loading(ModelLoadProgress),
    Ready(SolfegeModelInfo),
    Failed(ModelLoadError),
    Cancelled,
}

impl ModelLoadState {
    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading(_))
    }
}

/// Identity of a model file *as it is on disk right now*.
///
/// Keyed the same way the audio engine's `SfmRuntimeCacheKey` is, so the
/// inspector and the engine cannot disagree about the same path after a file
/// is replaced. Dropping a still-copying `.sfm` into the model folder produces
/// a checksum failure; once the copy finishes the stamp changes and the load
/// is retried instead of the failure sticking until restart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModelStamp {
    len: u64,
    modified_nanos: u128,
}

impl ModelStamp {
    fn read(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            len: metadata.len(),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |since_epoch| since_epoch.as_nanos()),
        })
    }
}

struct LoadEntry {
    stamp: ModelStamp,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<ModelLoadState>>,
}

impl LoadEntry {
    fn snapshot(&self) -> ModelLoadState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn abandon(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

static LOADS: OnceLock<Mutex<HashMap<PathBuf, LoadEntry>>> = OnceLock::new();
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
static REPAINT_POLL_RUNNING: AtomicBool = AtomicBool::new(false);

fn loads() -> MutexGuard<'static, HashMap<PathBuf, LoadEntry>> {
    LOADS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The current state of `path`, starting a background load on the first
/// request and whenever the file on disk has changed since the cached result.
///
/// Never blocks on I/O beyond a `stat`, so it is safe to call from a render
/// pass. Because the inspector shows one model at a time, requesting a
/// different model abandons any load still running for another path rather
/// than letting two 146 MB verifications compete for the disk.
pub fn model_load_state(path: &Path) -> ModelLoadState {
    let stamp = match ModelStamp::read(path) {
        Ok(stamp) => stamp,
        Err(error) => {
            return ModelLoadState::Failed(ModelLoadError {
                model: display_name(path),
                stage: ModelLoadStage::Opening,
                cause: format!("cannot read {}: {error}", path.display()),
            });
        }
    };

    let mut loads = loads();
    cancel_loads_except(&mut loads, path);
    if loads.get(path).is_some_and(|entry| entry.stamp == stamp) {
        return loads[path].snapshot();
    }
    // The file was replaced under a cached result; that result now describes
    // different bytes and must not be shown.
    if let Some(stale) = loads.remove(path) {
        stale.abandon();
    }

    let entry = start_load(path, stamp);
    let state = entry.snapshot();
    loads.insert(path.to_path_buf(), entry);
    state
}

/// Abandon an in-flight load for `path` and keep it abandoned.
///
/// The worker stops at its next chunk boundary and publishes `Cancelled`; it
/// can never publish a [`SolfegeModelInfo`] afterwards, so a cancelled load
/// leaves nothing half-installed. [`retry_model_load`] is the way back.
pub fn cancel_model_load(path: &Path) {
    let loads = loads();
    if let Some(entry) = loads.get(path) {
        entry.cancel.store(true, Ordering::Release);
        *entry
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ModelLoadState::Cancelled;
    }
}

/// Forget the cached outcome for `path` so the next [`model_load_state`] call
/// starts a fresh load.
pub fn retry_model_load(path: &Path) {
    if let Some(entry) = loads().remove(path) {
        entry.abandon();
    }
}

/// True while any model load is running. Drives the panel's repaint poll.
pub fn any_load_in_flight() -> bool {
    IN_FLIGHT.load(Ordering::Acquire) > 0
}

/// Keep windows repainting while a load publishes progress from its worker
/// thread. Cheap to call every frame: it starts at most one poller, and that
/// poller stops itself as soon as the last load finishes.
pub fn ensure_progress_repaint(cx: &mut App) {
    if !any_load_in_flight() || REPAINT_POLL_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(PROGRESS_POLL).await;
            cx.refresh();
            // Refresh first, then test: the frame that shows the finished
            // state is the one that ends the loop.
            if !any_load_in_flight() {
                break;
            }
        }
        REPAINT_POLL_RUNNING.store(false, Ordering::Release);
    })
    .detach();
}

/// Blocking load for callers that cannot yet defer — shares the same stamped
/// cache as the background path, so a model already inspected costs nothing.
pub(super) fn load_blocking(path: &Path) -> Result<SolfegeModelInfo, String> {
    let stamp = ModelStamp::read(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if let Some(entry) = loads().get(path) {
        if entry.stamp == stamp {
            match entry.snapshot() {
                ModelLoadState::Ready(info) => return Ok(info),
                ModelLoadState::Failed(error) => return Err(error.to_string()),
                ModelLoadState::Loading(_) | ModelLoadState::Cancelled => {}
            }
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(ModelLoadState::Loading(ModelLoadProgress {
        stage: ModelLoadStage::Opening,
        fraction: 0.0,
    })));
    let mut reporter = Reporter::new(
        state.clone(),
        cancel.clone(),
        LoadPlan::provisional(stamp.len),
    );
    let outcome = finish(load_model(path, stamp.len, &mut reporter), path);
    *state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = outcome.clone();
    loads().insert(
        path.to_path_buf(),
        LoadEntry {
            stamp,
            cancel,
            state,
        },
    );
    match outcome {
        ModelLoadState::Ready(info) => Ok(info),
        ModelLoadState::Failed(error) => Err(error.to_string()),
        ModelLoadState::Loading(_) | ModelLoadState::Cancelled => {
            Err(format!("{} load was cancelled.", display_name(path)))
        }
    }
}

/// Stop any load still reading a model other than `keep`. Finished results are
/// left alone so switching back to a model already inspected is instant.
fn cancel_loads_except(loads: &mut HashMap<PathBuf, LoadEntry>, keep: &Path) {
    loads.retain(|path, entry| {
        if path.as_path() == keep || !entry.snapshot().is_loading() {
            return true;
        }
        entry.abandon();
        false
    });
}

fn start_load(path: &Path, stamp: ModelStamp) -> LoadEntry {
    let cancel = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(ModelLoadState::Loading(ModelLoadProgress {
        stage: ModelLoadStage::Opening,
        fraction: 0.0,
    })));
    let entry = LoadEntry {
        stamp: stamp.clone(),
        cancel: cancel.clone(),
        state: state.clone(),
    };

    IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    let owned_path = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("solfege-model-load".to_string())
        .spawn(move || {
            let worker_state = state;
            let mut reporter = Reporter::new(
                worker_state.clone(),
                cancel,
                LoadPlan::provisional(stamp.len),
            );
            let outcome = finish(
                load_model(&owned_path, stamp.len, &mut reporter),
                &owned_path,
            );
            // The single publication point: a failed or cancelled load writes
            // its outcome here and never a partial model.
            *worker_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = outcome;
            IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        });
    if spawned.is_err() {
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        *entry
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ModelLoadState::Failed(ModelLoadError {
                model: display_name(path),
                stage: ModelLoadStage::Opening,
                cause: "no worker thread available for the model load".to_string(),
            });
    }
    entry
}

fn finish(outcome: Result<SolfegeModelInfo, StageFailure>, path: &Path) -> ModelLoadState {
    match outcome {
        Ok(info) => ModelLoadState::Ready(info),
        Err(StageFailure::Cancelled) => ModelLoadState::Cancelled,
        Err(StageFailure::Failed { stage, cause }) => ModelLoadState::Failed(ModelLoadError {
            model: display_name(path),
            stage,
            cause,
        }),
    }
}

fn display_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("Solfege model")
        .to_string()
}

// ---------------------------------------------------------------------------
// Work plan and progress reporting
// ---------------------------------------------------------------------------

/// How many bytes each stage of a load will move or hash.
///
/// Derived from the section table of the file being opened, so the shape of
/// the bar follows the shape of the package: a voicebank-heavy model spends
/// its time in `Validating`, an FBMX-only one in `LoadingNeuralModel`, and
/// neither stalls on a stage it has no work for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LoadPlan {
    read: u64,
    verify: u64,
    voicebank: u64,
    physical: u64,
    neural: u64,
    assemble: u64,
}

impl LoadPlan {
    /// Before the header is in memory only the file length is known. Verify is
    /// an upper bound (body + sections, and sections cannot exceed the body),
    /// which the exact plan replaces as soon as the section table is read.
    fn provisional(file_size: u64) -> Self {
        Self {
            read: file_size,
            verify: file_size.saturating_mul(2),
            ..Self::default()
        }
    }

    /// The exact plan for an SFM package.
    ///
    /// `verify` matches `SfmFile`'s own denominator exactly: the body digest
    /// plus one digest per section. `neural` counts the residual twice because
    /// `FbmxModel::from_bytes` digests its body once for the container and
    /// once for the weight table.
    fn for_sfm(file_size: u64, sections: &[solfege_model::SectionIndex]) -> Self {
        let size_of = |tag: [u8; 4]| {
            sections
                .iter()
                .find(|entry| entry.tag == tag)
                .map_or(0, |entry| entry.size)
        };
        let section_bytes = sections
            .iter()
            .fold(0u64, |sum, entry| sum.saturating_add(entry.size));
        Self {
            read: file_size,
            verify: file_size
                .saturating_sub(solfege_model::TRAILER_SIZE as u64)
                .saturating_add(section_bytes),
            voicebank: size_of(solfege_model::METADATA_TAG)
                .saturating_add(size_of(solfege_model::ACOUSTIC_TAG))
                .saturating_add(size_of(solfege_model::INDEX_TAG)),
            physical: size_of(solfege_model::PHYSICAL_TAG),
            neural: size_of(solfege_model::FBMX_RESIDUAL_TAG).saturating_mul(2),
            assemble: 0,
        }
    }

    /// A standalone `.fbmx` has no section table to weigh; its container digest
    /// is the whole of the work after the read.
    fn for_fbmx(file_size: u64) -> Self {
        Self {
            read: file_size,
            neural: file_size.saturating_mul(2),
            ..Self::default()
        }
    }

    fn total(&self) -> u64 {
        self.completed_before(ModelLoadStage::PreparingEngine)
            .saturating_add(self.assemble)
            .max(1)
    }

    fn completed_before(&self, stage: ModelLoadStage) -> u64 {
        let mut done = 0u64;
        for (weight, boundary) in [
            (self.read, ModelLoadStage::Validating),
            (self.verify, ModelLoadStage::LoadingVoicebank),
            (self.voicebank, ModelLoadStage::LoadingPhysicalModel),
            (self.physical, ModelLoadStage::LoadingNeuralModel),
            (self.neural, ModelLoadStage::PreparingEngine),
            (self.assemble, ModelLoadStage::Ready),
        ] {
            if stage == boundary {
                return done.saturating_add(weight);
            }
            done = done.saturating_add(weight);
        }
        // Opening is the only stage with nothing completed before it.
        0
    }
}

/// Publishes progress into the shared slot the UI reads, and answers the
/// cancellation question for every checkpoint in the load.
struct Reporter {
    state: Arc<Mutex<ModelLoadState>>,
    cancel: Arc<AtomicBool>,
    plan: LoadPlan,
    stage: ModelLoadStage,
    /// Highest fraction published so far. The plan is refined once, when the
    /// section table replaces the provisional estimate; clamping here keeps
    /// the bar from stepping backwards at that moment.
    published: f32,
}

impl Reporter {
    fn new(state: Arc<Mutex<ModelLoadState>>, cancel: Arc<AtomicBool>, plan: LoadPlan) -> Self {
        Self {
            state,
            cancel,
            plan,
            stage: ModelLoadStage::Opening,
            published: 0.0,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    fn checkpoint(&self) -> Result<(), StageFailure> {
        if self.cancelled() {
            return Err(StageFailure::Cancelled);
        }
        Ok(())
    }

    fn set_plan(&mut self, plan: LoadPlan) {
        self.plan = plan;
    }

    fn enter(&mut self, stage: ModelLoadStage) -> Result<(), StageFailure> {
        self.checkpoint()?;
        self.stage = stage;
        self.publish(0);
        Ok(())
    }

    fn publish(&mut self, done_in_stage: u64) {
        let done = self.plan.completed_before(self.stage) + done_in_stage;
        let fraction = (done as f64 / self.plan.total() as f64) as f32;
        self.published = self.published.max(fraction).clamp(0.0, 1.0);
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            ModelLoadState::Loading(ModelLoadProgress {
                stage: self.stage,
                fraction: self.published,
            });
    }
}

impl solfege_model::SfmLoadObserver for Reporter {
    fn verified(&mut self, hashed: u64, _total: u64) {
        self.publish(hashed);
    }

    fn cancelled(&mut self) -> bool {
        Reporter::cancelled(self)
    }
}

#[derive(Debug)]
enum StageFailure {
    Cancelled,
    Failed {
        stage: ModelLoadStage,
        cause: String,
    },
}

fn failed(stage: ModelLoadStage, cause: impl Into<String>) -> StageFailure {
    StageFailure::Failed {
        stage,
        cause: cause.into(),
    }
}

/// An SFM error maps to the stage that produced it; a cancellation is not a
/// failure and must not be reported as one.
fn from_sfm(stage: ModelLoadStage, error: solfege_model::SfmError) -> StageFailure {
    match error {
        solfege_model::SfmError::Cancelled => StageFailure::Cancelled,
        solfege_model::SfmError::Io(_) => failed(ModelLoadStage::Opening, error.to_string()),
        error => failed(stage, error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// The load itself
// ---------------------------------------------------------------------------

fn load_model(
    path: &Path,
    file_size: u64,
    reporter: &mut Reporter,
) -> Result<SolfegeModelInfo, StageFailure> {
    let is_sfm = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("sfm"));
    let info = if is_sfm {
        load_sfm(path, file_size, reporter)?
    } else {
        load_fbmx(path, file_size, reporter)?
    };
    reporter.enter(ModelLoadStage::Ready)?;
    Ok(info)
}

/// Read the whole file, reporting bytes as they land and stopping at the first
/// chunk boundary after a cancellation.
fn read_reported(
    path: &Path,
    file_size: u64,
    reporter: &mut Reporter,
) -> Result<Vec<u8>, StageFailure> {
    reporter.enter(ModelLoadStage::Opening)?;
    let mut file = File::open(path)
        .map_err(|error| failed(ModelLoadStage::Opening, format!("cannot open: {error}")))?;
    let mut bytes = Vec::with_capacity(file_size as usize);
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        reporter.checkpoint()?;
        let read = file
            .read(&mut chunk)
            .map_err(|error| failed(ModelLoadStage::Opening, format!("cannot read: {error}")))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        reporter.publish(bytes.len() as u64);
    }
    Ok(bytes)
}

fn load_sfm(
    path: &Path,
    file_size: u64,
    reporter: &mut Reporter,
) -> Result<SolfegeModelInfo, StageFailure> {
    let bytes = read_reported(path, file_size, reporter)?;

    // The section table is cheap and comes before any digest, so the remaining
    // stages can be weighted by the bytes they will really touch.
    let sections = solfege_model::SfmFile::peek_sections(&bytes)
        .map_err(|error| from_sfm(ModelLoadStage::Validating, error))?;
    reporter.set_plan(LoadPlan::for_sfm(bytes.len() as u64, &sections));

    reporter.enter(ModelLoadStage::Validating)?;
    let model = solfege_model::SfmFile::from_vec_observed(bytes, reporter)
        .map_err(|error| from_sfm(ModelLoadStage::Validating, error))?;

    reporter.enter(ModelLoadStage::LoadingVoicebank)?;
    let metadata = model
        .metadata_json()
        .map_err(|error| from_sfm(ModelLoadStage::LoadingVoicebank, error))?;
    let index_present = model.section(solfege_model::INDEX_TAG).is_some();
    let audio_present = model.section(solfege_model::AUDIO_TAG).is_some();
    if index_present != audio_present {
        return Err(failed(
            ModelLoadStage::LoadingVoicebank,
            format!(
                "incomplete voicebank: INDX {}, AUDO {}",
                if index_present { "present" } else { "missing" },
                if audio_present { "present" } else { "missing" },
            ),
        ));
    }
    let acoustic_metadata = model
        .section(solfege_model::ACOUSTIC_TAG)
        .map(|raw| {
            serde_json::from_slice::<serde_json::Value>(raw)
                .map_err(|error| failed(ModelLoadStage::LoadingVoicebank, error.to_string()))
        })
        .transpose()?;
    let voicebank_metadata = index_present.then(|| metadata.get("voicebank")).flatten();
    let voicebank_profiles = acoustic_metadata
        .as_ref()
        .and_then(|value| value.get("profiles"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    let voicebank_entries = voicebank_metadata
        .and_then(|value| value.get("entries"))
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            acoustic_metadata
                .as_ref()
                .and_then(|value| value.get("entries"))
                .and_then(serde_json::Value::as_array)
                .map(|entries| entries.len() as u64)
        })
        .unwrap_or(0) as usize;
    let voicebank_audio_bytes = voicebank_metadata
        .and_then(|value| value.get("audio_bytes"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let voicebank_source_files = acoustic_metadata
        .as_ref()
        .and_then(|value| value.get("source_file_count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;

    reporter.enter(ModelLoadStage::LoadingPhysicalModel)?;
    let profile = model
        .physical_profile()
        .map_err(|error| from_sfm(ModelLoadStage::LoadingPhysicalModel, error))?;

    reporter.enter(ModelLoadStage::LoadingNeuralModel)?;
    let residual_info = model
        .section(solfege_model::FBMX_RESIDUAL_TAG)
        .map(|raw| {
            fbmx_runtime::FbmxModel::from_bytes(raw)
                .map_err(|error| failed(ModelLoadStage::LoadingNeuralModel, error.to_string()))
        })
        .transpose()?;

    // The Performer is read in the same stage as the residual: both are FBMX
    // sections and both are small next to the voicebank. A model whose
    // Performer this build cannot run reports no Performer rather than failing
    // the load — the instrument still plays, it just plays what it is given.
    let performer_info = model
        .section(solfege_model::FBMX_PERFORMER_TAG)
        .and_then(|raw| fbmx_runtime::FbmxModel::from_bytes(raw).ok())
        .and_then(|performer| {
            let runtime = performer.instantiate_performer().ok()?;
            Some(super::SolfegePerformerInfo {
                input_size: runtime.input_size(),
                output_size: runtime.output_size(),
                bidirectional: runtime.is_bidirectional(),
                weight_bytes: performer.weight_bytes() as u64,
                feature_schema_version: performer_feature_schema(&performer),
            })
        });

    // The Accent Analyzer, if the package carries one. Most do not, and that is
    // the designed state rather than a missing piece: Analyze Accent runs on
    // the built-in fitted rule and only takes a model where one has been shown
    // to beat it.
    let accent_info = model
        .section(solfege_model::FBMX_ACCENT_TAG)
        .and_then(|raw| fbmx_runtime::FbmxModel::from_bytes(raw).ok())
        .map(|accent| {
            let runtime = accent.instantiate_accent_analyzer().ok();
            super::SolfegeAccentInfo {
                input_size: runtime.as_ref().map_or(0, |r| r.input_size()),
                output_size: runtime.as_ref().map_or(0, |r| r.output_size()),
                weight_bytes: accent.weight_bytes() as u64,
                usable: runtime.is_some(),
            }
        });

    reporter.enter(ModelLoadStage::PreparingEngine)?;
    Ok(SolfegeModelInfo {
        name: metadata
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| display_name(path)),
        model_type: "SFM v1 / neural voicebank".to_string(),
        architecture: match (index_present, residual_info.is_some()) {
            (true, true) => "Neural voicebank + BowedString + embedded FBMX residual",
            (true, false) => "Neural voicebank + BowedString",
            (false, true) => "BowedString + embedded FBMX residual",
            (false, false) => "BowedString physical",
        }
        .to_string(),
        sample_rate: profile.sample_rate,
        source_type: metadata
            .get("model_source_type")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                metadata
                    .get("source")
                    .and_then(|source| source.get("dataset"))
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap_or("unknown")
            .to_string(),
        validated: metadata
            .get("validated")
            .and_then(serde_json::Value::as_bool)
            .or_else(|| {
                metadata
                    .get("fbmx")
                    .and_then(|fbmx| fbmx.get("validated"))
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false),
        parameter_count: residual_info
            .as_ref()
            .map_or(0, |model| model.info().architecture.parameter_count),
        file_size_bytes: file_size,
        voicebank_profiles,
        voicebank_entries,
        voicebank_audio_bytes,
        voicebank_source_files,
        performer: performer_info,
        accent: accent_info,
    })
}

/// Which feature vocabulary an embedded Performer was trained against.
///
/// Written by the export pipeline into the container's `extra` block. A file
/// that does not carry it predates the field and is schema 1 — the sixteen
/// score features, with no accent inputs.
fn performer_feature_schema(model: &fbmx_runtime::FbmxModel) -> u32 {
    model
        .header()
        .extra
        .get("performer")
        .and_then(|performer| performer.get("feature_schema_version"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1) as u32
}

fn load_fbmx(
    path: &Path,
    file_size: u64,
    reporter: &mut Reporter,
) -> Result<SolfegeModelInfo, StageFailure> {
    let bytes = read_reported(path, file_size, reporter)?;
    reporter.set_plan(LoadPlan::for_fbmx(bytes.len() as u64));

    reporter.enter(ModelLoadStage::LoadingNeuralModel)?;
    let model = fbmx_runtime::FbmxModel::from_bytes(&bytes)
        .map_err(|error| failed(ModelLoadStage::LoadingNeuralModel, error.to_string()))?;

    reporter.enter(ModelLoadStage::PreparingEngine)?;
    let info = model.info();
    Ok(SolfegeModelInfo {
        name: if info.name.is_empty() {
            display_name(path)
        } else {
            info.name.clone()
        },
        model_type: info.model_type.as_str().to_string(),
        architecture: info.architecture.name.clone(),
        sample_rate: info.sample_rate,
        source_type: info.source_type.as_str().to_string(),
        validated: info.validated,
        parameter_count: info.architecture.parameter_count,
        file_size_bytes: file_size,
        voicebank_profiles: 0,
        voicebank_entries: 0,
        voicebank_audio_bytes: 0,
        voicebank_source_files: 0,
        // A standalone `.fbmx` reaching this path is an audio model: the
        // discovery pass offers neither Performers nor Accent Analyzers as
        // instruments, so there is nothing to report for either.
        performer: None,
        accent: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sfm_sections(sizes: &[([u8; 4], u64)]) -> Vec<solfege_model::SectionIndex> {
        sizes
            .iter()
            .scan(64u64, |offset, (tag, size)| {
                let entry = solfege_model::SectionIndex {
                    tag: *tag,
                    flags: 0,
                    offset: *offset,
                    size: *size,
                    checksum: [0u8; 32],
                };
                *offset += size;
                Some(entry)
            })
            .collect()
    }

    #[test]
    fn voicebank_package_spends_its_budget_on_reading_and_hashing() {
        // The shipped SoloViolin shape: a 146 MB AUDO section and kilobytes of
        // everything else.
        let audio = 146_391_980u64;
        let sections = sfm_sections(&[
            (solfege_model::METADATA_TAG, 2_048),
            (solfege_model::PHYSICAL_TAG, 1_024),
            (solfege_model::ACOUSTIC_TAG, 8_192),
            (solfege_model::INDEX_TAG, 12_288),
            (solfege_model::AUDIO_TAG, audio),
        ]);
        let file_size = 146_643_684u64;
        let plan = LoadPlan::for_sfm(file_size, &sections);

        let hashing_share = (plan.read + plan.verify) as f64 / plan.total() as f64;
        assert!(
            hashing_share > 0.99,
            "read+verify should dominate a voicebank package, got {hashing_share}"
        );
        // Validating ends within a hair of full for this shape, which is what
        // makes the bar honest: the AUDO digest really is the load.
        let validating_end = plan.completed_before(ModelLoadStage::LoadingVoicebank);
        assert!(validating_end as f64 / plan.total() as f64 > 0.99);
    }

    #[test]
    fn physical_only_package_does_not_stall_on_the_voicebank_stage() {
        let sections = sfm_sections(&[
            (solfege_model::METADATA_TAG, 2_048),
            (solfege_model::PHYSICAL_TAG, 1_024),
            (solfege_model::FBMX_RESIDUAL_TAG, 4_000_000),
        ]);
        let plan = LoadPlan::for_sfm(4_100_000, &sections);
        assert_eq!(plan.voicebank, 2_048, "no INDX or ACOU to read");
        // The residual is the only heavy stage left, so the bar keeps moving
        // through it instead of parking where a voicebank would have been.
        assert!(plan.neural > plan.total() / 4);
    }

    #[test]
    fn progress_never_rewinds_when_the_provisional_plan_is_replaced() {
        let state = Arc::new(Mutex::new(ModelLoadState::Cancelled));
        let mut reporter = Reporter::new(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            LoadPlan::provisional(1_000_000),
        );
        reporter.enter(ModelLoadStage::Opening).unwrap();
        reporter.publish(1_000_000);
        let after_read = reporter.published;
        assert!(after_read > 0.0);

        // The exact plan is smaller than the upper bound, which would push the
        // same byte count to a *higher* fraction; the clamp only has to hold
        // the line when it is the other way around.
        reporter.set_plan(LoadPlan::provisional(4_000_000));
        reporter.enter(ModelLoadStage::Validating).unwrap();
        assert!(reporter.published >= after_read);
    }

    #[test]
    fn a_cancelled_reporter_stops_at_the_next_stage_boundary() {
        let cancel = Arc::new(AtomicBool::new(true));
        let mut reporter = Reporter::new(
            Arc::new(Mutex::new(ModelLoadState::Cancelled)),
            cancel,
            LoadPlan::provisional(10),
        );
        assert!(matches!(
            reporter.enter(ModelLoadStage::Validating),
            Err(StageFailure::Cancelled)
        ));
    }

    #[test]
    fn failure_messages_name_the_model_and_the_cause() {
        let error = ModelLoadError {
            model: "Solo Violin".to_string(),
            stage: ModelLoadStage::Validating,
            cause: "AUDO section checksum mismatch".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "Solo Violin failed to load: AUDO section checksum mismatch."
        );
    }

    #[test]
    fn a_missing_file_reports_the_opening_stage_without_blocking() {
        let state = model_load_state(Path::new("W:/works/Futureboard/does-not-exist.sfm"));
        match state {
            ModelLoadState::Failed(error) => {
                assert_eq!(error.stage, ModelLoadStage::Opening);
                assert!(error.cause.contains("cannot read"));
            }
            other => panic!("expected a failed load, got {other:?}"),
        }
    }
}

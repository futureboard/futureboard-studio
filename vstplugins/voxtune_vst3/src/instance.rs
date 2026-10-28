//! What one plug-in instance shares between the audio thread, the editor
//! (GPUI's thread) and ARA (the host's main thread).
//!
//! The audio thread only reads atomics and `try_lock`s; everything that
//! allocates — decoding, analysis, swapping in a new engine — happens on a
//! loader thread or the caller's. An engine the audio thread may still hold
//! is never freed there: a swapped-out engine is parked in `retired` until
//! nothing else holds it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sphere_voxtune::audio_io::decode_audio_file_to_mono;
use sphere_voxtune::engine::{analyze_samples, AnalysisProgress, CantoAnalysisResult, CantoEngine};
use sphere_voxtune::session::VoxDocument;
use sphere_voxtune::shared::Shared;
use sphere_voxtune::store::TakeStore;

/// The take a non-ARA instance plays, as saved with the host's project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedTake {
    pub path: PathBuf,
    /// Host time, in seconds, where the take's first sample plays.
    pub offset_sec: f64,
    /// The edits.
    pub doc: Option<VoxDocument>,
}

/// What the loader thread is doing.
#[derive(Debug, Clone, Default)]
pub enum LoadStatus {
    #[default]
    Idle,
    Loading {
        title: String,
        progress: Arc<AnalysisProgress>,
    },
    Failed(String),
}

pub struct Instance {
    /// The take's engine (non-ARA), at the host's rate.
    engine: Mutex<Arc<Shared>>,
    /// Engines swapped out while the audio thread may still hold them.
    retired: Mutex<Vec<Arc<Shared>>>,
    /// Bumped whenever `engine` (or its take) changes.
    pub generation: AtomicU64,
    pub host_rate: AtomicU32,
    /// Host transport, published by every process call.
    pub playing: AtomicBool,
    position_bits: AtomicU64,
    /// Take start in host seconds.
    offset_bits: AtomicU64,
    /// Length of the loaded take (0: none).
    take_len_bits: AtomicU64,
    pub load: Mutex<LoadStatus>,
    /// Bumped when `load` changes, for the editor.
    pub load_generation: AtomicU64,
    /// ARA binding of this instance, once the host made one.
    pub ara: Mutex<Option<Arc<crate::ara::BindingShared>>>,
}

impl Default for Instance {
    fn default() -> Self {
        Self {
            engine: Mutex::new(Arc::new(Shared::new(CantoEngine::new(48_000.0)))),
            retired: Mutex::new(Vec::new()),
            generation: AtomicU64::new(1),
            host_rate: AtomicU32::new(0),
            playing: AtomicBool::new(false),
            position_bits: AtomicU64::new(0f64.to_bits()),
            offset_bits: AtomicU64::new(0f64.to_bits()),
            take_len_bits: AtomicU64::new(0f64.to_bits()),
            load: Mutex::new(LoadStatus::Idle),
            load_generation: AtomicU64::new(0),
            ara: Mutex::new(None),
        }
    }
}

/// Where analyses and edits are cached — the same folder Studio and the
/// desktop app use, so a take analysed in one opens instantly in the others.
pub fn take_store() -> TakeStore {
    TakeStore::new(
        crate::paths::FutureboardPaths::resolve()
            .app_data
            .join("Cache")
            .join("Voxtune"),
    )
}

impl Instance {
    pub fn engine(&self) -> Arc<Shared> {
        self.engine.lock().clone()
    }

    /// The engine for the audio thread: None when the lock is busy (keep
    /// the previous one for this block).
    pub fn try_engine(&self) -> Option<Arc<Shared>> {
        self.engine.try_lock().map(|e| e.clone())
    }

    pub fn publish_transport(&self, playing: bool, position: f64) {
        self.playing.store(playing, Ordering::Relaxed);
        self.position_bits
            .store(position.to_bits(), Ordering::Relaxed);
    }

    pub fn position(&self) -> f64 {
        f64::from_bits(self.position_bits.load(Ordering::Relaxed))
    }

    pub fn offset(&self) -> f64 {
        f64::from_bits(self.offset_bits.load(Ordering::Relaxed))
    }

    pub fn set_offset(&self, seconds: f64) {
        self.offset_bits.store(seconds.to_bits(), Ordering::Relaxed);
    }

    pub fn take_len(&self) -> f64 {
        f64::from_bits(self.take_len_bits.load(Ordering::Relaxed))
    }

    pub fn ara_bound(&self) -> bool {
        self.ara.lock().is_some()
    }

    fn set_load(&self, status: LoadStatus) {
        *self.load.lock() = status;
        self.load_generation.fetch_add(1, Ordering::Release);
    }

    /// Swaps in `shared` (not on the audio thread).
    fn install(&self, shared: Arc<Shared>, take_len: f64) {
        let old = std::mem::replace(&mut *self.engine.lock(), shared);
        self.take_len_bits
            .store(take_len.to_bits(), Ordering::Relaxed);
        let mut retired = self.retired.lock();
        retired.retain(|e| Arc::strong_count(e) > 1);
        retired.push(old);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Loads `path` on a thread of its own at the host's rate: the saved
    /// edits `doc` when they fit the file (no analysis needed), else the
    /// cached analysis, else a fresh one. Not realtime.
    pub fn load_take(self: &Arc<Self>, path: PathBuf, offset_sec: f64, doc: Option<VoxDocument>) {
        let title = file_name(&path);
        let progress = Arc::new(AnalysisProgress::default());
        self.set_offset(offset_sec);
        self.set_load(LoadStatus::Loading {
            title: title.clone(),
            progress: progress.clone(),
        });
        let this = self.clone();
        let spawned = std::thread::Builder::new()
            .name("voxtune-take".into())
            .spawn(move || match load(&this, &path, doc, &progress) {
                Ok((shared, len)) => {
                    this.install(shared, len);
                    this.set_load(LoadStatus::Idle);
                }
                Err(e) => this.set_load(LoadStatus::Failed(format!("{title}: {e}"))),
            });
        if let Err(e) = spawned {
            self.set_load(LoadStatus::Failed(format!("Could not start loading: {e}")));
        }
    }

    /// The host's rate changed: the take is loaded again at the new one.
    pub fn rate(&self) -> u32 {
        match self.host_rate.load(Ordering::Relaxed) {
            0 => 48_000,
            r => r,
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn load(
    instance: &Instance,
    path: &Path,
    doc: Option<VoxDocument>,
    progress: &AnalysisProgress,
) -> Result<(Arc<Shared>, f64), String> {
    let rate = instance.rate();
    let decoded = decode_audio_file_to_mono(path, rate)?;
    let duration = decoded.duration_sec;
    let mut engine = CantoEngine::new(rate as f32);
    let store = take_store();
    let fits = |d: &VoxDocument| {
        !d.notes.is_empty()
            && d.notes
                .iter()
                .all(|n| (n.detected_start + n.detected_dur) as f64 <= duration + 0.1)
    };
    match doc.filter(fits) {
        Some(doc) => {
            engine.install_analyzed(decoded, doc.to_analysis());
            doc.apply_to(&mut engine);
        }
        None => {
            let cached = store
                .read_analysis(path)
                .and_then(|json| serde_json::from_str::<CantoAnalysisResult>(&json).ok());
            let restored = match cached {
                Some(result) => engine
                    .install_decoded_with_analysis(decoded.clone(), result)
                    .is_ok(),
                None => false,
            };
            if !restored {
                let analysis = analyze_samples(&decoded.samples, rate as f32, progress)
                    .ok_or("analysis was cancelled")?;
                let result = engine.install_analyzed(decoded, analysis);
                if let Ok(json) = serde_json::to_string(&result) {
                    let _ = store.write_analysis(path, &json);
                }
            }
        }
    }
    Ok((Arc::new(Shared::new(engine)), duration))
}

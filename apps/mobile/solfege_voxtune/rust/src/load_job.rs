//! Opening a file without blocking the UI: decoding and analysis (or
//! adopting a cached analysis) run on a worker thread with live progress and
//! cancel. Only the final swap into the engine takes its lock, briefly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::audio_io::decode_audio_file_to_mono;
use crate::engine::{Analysis, AnalysisProgress, CantoAnalysisResult, analyze_samples};
use crate::output::Shared;

pub const STAGE_DECODING: u32 = 0;
pub const STAGE_ANALYZING: u32 = 1;
pub const STAGE_INSTALLING: u32 = 2;
pub const STAGE_DONE: u32 = 3;

pub enum JobKind {
    /// Decode a file; adopt `cached` (an earlier result's JSON) if it fits.
    File {
        path: PathBuf,
        cached: Option<String>,
    },
    /// Analyse the audio already in the engine again.
    Reanalyze,
}

#[derive(Default)]
pub struct JobState {
    pub progress: AnalysisProgress,
    pub stage: AtomicU32,
    pub used_cache: AtomicBool,
    result: Mutex<Option<Result<CantoAnalysisResult, String>>>,
}

impl JobState {
    pub fn is_done(&self) -> bool {
        self.stage.load(Ordering::Acquire) == STAGE_DONE
    }

    /// The outcome, once; None while running or after it was taken.
    pub fn take_result(&self) -> Option<Result<CantoAnalysisResult, String>> {
        if !self.is_done() {
            return None;
        }
        self.result.lock().ok()?.take()
    }
}

/// A running load. Dropping it cancels the work; the worker keeps the
/// engine state alive (`Arc<Shared>`) until it has stopped, so the engine
/// handle can be freed at any time.
pub struct LoadJob {
    pub state: Arc<JobState>,
}

impl LoadJob {
    pub fn start(shared: Arc<Shared>, kind: JobKind) -> Self {
        let state = Arc::new(JobState::default());
        let worker = Arc::clone(&state);
        let spawned = thread::Builder::new()
            .name("canto-load".into())
            .spawn(move || {
                let res = run(&shared, kind, &worker);
                finish(&worker, res);
            });
        if let Err(e) = spawned {
            finish(&state, Err(format!("Could not start loading: {e}")));
        }
        Self { state }
    }

    pub fn cancel(&self) {
        self.state.progress.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for LoadJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_io::encode_mono_wav_i16;
    use crate::engine::CantoEngine;
    use std::time::{Duration, Instant};

    fn wav_file(secs: f32) -> PathBuf {
        let sr = 48_000u32;
        let mono: Vec<f32> = (0..(sr as f32 * secs) as usize)
            .map(|i| {
                let t = i as f32 / sr as f32;
                let hz = if (t % 1.0) < 0.5 { 220.0 } else { 261.63 };
                if (t % 0.5) < 0.42 {
                    0.45 * (std::f32::consts::TAU * hz * t).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let path =
            std::env::temp_dir().join(format!("canto_job_{}_{secs}.wav", std::process::id()));
        std::fs::write(&path, encode_mono_wav_i16(&mono, sr)).unwrap();
        path
    }

    fn wait(job: &LoadJob) -> Result<CantoAnalysisResult, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(res) = job.state.take_result() {
                return res;
            }
            assert!(Instant::now() < deadline, "job never finished");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn loads_off_thread_then_adopts_its_cached_analysis() {
        let path = wav_file(2.0);
        let shared = Arc::new(Shared::new(CantoEngine::new(48_000.0)));
        let job = LoadJob::start(
            Arc::clone(&shared),
            JobKind::File {
                path: path.clone(),
                cached: None,
            },
        );
        let fresh = wait(&job).expect("load");
        assert!(!job.state.used_cache.load(Ordering::Relaxed));
        assert_eq!(fresh.notes.len(), 4);
        assert!(shared.lock().has_audio());

        let other = Arc::new(Shared::new(CantoEngine::new(48_000.0)));
        let job = LoadJob::start(
            Arc::clone(&other),
            JobKind::File {
                path,
                cached: Some(serde_json::to_string(&fresh).unwrap()),
            },
        );
        let cached = wait(&job).expect("cached load");
        assert!(job.state.used_cache.load(Ordering::Relaxed));
        assert_eq!(cached.notes, fresh.notes);
    }

    #[test]
    fn cancel_stops_analysis_early_and_leaves_the_engine_untouched() {
        let path = wav_file(60.0);
        let shared = Arc::new(Shared::new(CantoEngine::new(48_000.0)));
        let job = LoadJob::start(Arc::clone(&shared), JobKind::File { path, cached: None });
        // The engine stays usable (lock free) while the worker analyses.
        while job.state.stage.load(Ordering::Relaxed) != STAGE_ANALYZING {
            thread::sleep(Duration::from_millis(1));
        }
        let t = Instant::now();
        drop(shared.lock());
        assert!(t.elapsed() < Duration::from_millis(50));
        job.cancel();
        assert_eq!(wait(&job).unwrap_err(), "Cancelled");
        assert!(!shared.lock().has_audio());
    }
}

fn finish(state: &JobState, res: Result<CantoAnalysisResult, String>) {
    if let Ok(mut slot) = state.result.lock() {
        *slot = Some(res);
    }
    state.stage.store(STAGE_DONE, Ordering::Release);
}

fn cancelled(state: &JobState) -> bool {
    state.progress.cancel.load(Ordering::Relaxed)
}

fn run(shared: &Shared, kind: JobKind, state: &JobState) -> Result<CantoAnalysisResult, String> {
    let install = |f: &mut dyn FnMut(&mut crate::engine::CantoEngine) -> CantoAnalysisResult| {
        state.stage.store(STAGE_INSTALLING, Ordering::Relaxed);
        let mut engine = shared.lock();
        let res = f(&mut engine);
        shared.publish(&engine);
        res
    };
    match kind {
        JobKind::File { path, cached } => {
            state.stage.store(STAGE_DECODING, Ordering::Relaxed);
            let decoded = decode_audio_file_to_mono(&path, shared.sample_rate() as u32)?;
            if cancelled(state) {
                return Err("Cancelled".to_string());
            }
            let adopted = cached
                .and_then(|json| serde_json::from_str::<CantoAnalysisResult>(&json).ok())
                .and_then(|r| Analysis::from_cached(r, decoded.duration_sec));
            let analysis = match adopted {
                Some(a) => {
                    state.used_cache.store(true, Ordering::Relaxed);
                    a
                }
                None => {
                    state.stage.store(STAGE_ANALYZING, Ordering::Relaxed);
                    analyze_samples(&decoded.samples, shared.sample_rate(), &state.progress)
                        .ok_or_else(|| "Cancelled".to_string())?
                }
            };
            let mut slot = Some((decoded, analysis));
            Ok(install(&mut |e| {
                let (decoded, analysis) = slot.take().expect("installed once");
                e.install_analyzed(decoded, analysis)
            }))
        }
        JobKind::Reanalyze => {
            // Copy the samples so analysis doesn't hold the engine lock.
            let samples = shared.lock().audio_samples.clone();
            if samples.is_empty() {
                return Err("No audio loaded to analyze".to_string());
            }
            state.stage.store(STAGE_ANALYZING, Ordering::Relaxed);
            let analysis = analyze_samples(&samples, shared.sample_rate(), &state.progress)
                .ok_or_else(|| "Cancelled".to_string())?;
            let mut slot = Some(analysis);
            Ok(install(&mut |e| {
                e.adopt(slot.take().expect("installed once"))
            }))
        }
    }
}

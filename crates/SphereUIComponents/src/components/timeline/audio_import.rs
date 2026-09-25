//! Background audio import: probe → peak generation → disk cache.
//!
//! Never runs decode/peak work on the GPUI render thread. UI updates are
//! throttled to ≤10 Hz except on state transitions.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{AsyncApp, Context, Entity, WeakEntity};

use super::timeline::Timeline;
use super::timeline_state::AudioImportState;
use super::waveform_cache::{self, WaveformFileMeta, WaveformPreview, CHUNK_PEAKS, PEAK_FINE_SPP};
use super::waveform_peak_file::{
    read_peak_file, waveform_peak_relative_path_for_asset, write_peak_file, PeakFileError,
    SourceFingerprint,
};
use crate::layout::StudioLayout;
use crate::project::io::relative_path_in_project;

/// Bump when peak format or LOD ladder changes to invalidate disk cache.
pub const PEAK_DECODER_VERSION: u32 = waveform_cache::WAVEFORM_ALGORITHM_VERSION;
const TARGET_PEAK_SAMPLE_RATE: u32 = 48_000;

static NOTIFY_THROTTLE: OnceLock<Mutex<Instant>> = OnceLock::new();
static IMPORT_DEBUG: OnceLock<bool> = OnceLock::new();
static UI_NOTIFY_COUNT: OnceLock<Mutex<u64>> = OnceLock::new();

fn import_debug() -> bool {
    *IMPORT_DEBUG.get_or_init(|| std::env::var_os("FUTUREBOARD_AUDIO_IMPORT_DEBUG").is_some())
}

fn project_peak_path(project_root: &Path, asset_id: &str) -> PathBuf {
    let relative = waveform_peak_relative_path_for_asset(asset_id);
    project_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}

fn try_load_project_peak_cache(
    project_root: &Path,
    asset_id: &str,
    source_path: &Path,
) -> Option<Arc<WaveformPreview>> {
    let path = project_peak_path(project_root, asset_id);
    let expected_source = SourceFingerprint::for_path(source_path);
    match read_peak_file(&path, Some(asset_id), expected_source) {
        Ok(preview) => {
            let peak_count: usize = preview.lods.iter().map(|l| l.peaks.len()).sum();
            eprintln!(
                "[WaveformCache] disk hit path={} asset_id={asset_id} peaks={peak_count}",
                path.display()
            );
            Some(Arc::new(preview))
        }
        Err(PeakFileError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("[WaveformCache] disk miss asset_id={asset_id}");
            None
        }
        Err(PeakFileError::SourceChanged { .. }) => {
            eprintln!("[WaveformCache] source changed; regenerating asset_id={asset_id}");
            None
        }
        Err(err) => {
            eprintln!(
                "[WaveformCache] corrupt peak file; regenerating path={} error={err}",
                path.display()
            );
            None
        }
    }
}

fn save_project_peak_cache(
    project_root: &Path,
    asset_id: &str,
    preview: &WaveformPreview,
    source: Option<SourceFingerprint>,
) -> Option<PathBuf> {
    let path = project_peak_path(project_root, asset_id);
    match write_peak_file(&path, asset_id, preview, source) {
        Ok(bytes) => {
            eprintln!(
                "[WaveformCache] written path={} bytes={bytes}",
                path.display()
            );
            Some(path)
        }
        Err(err) => {
            eprintln!(
                "[WaveformCache] write failed asset_id={asset_id} path={} error={err}",
                path.display()
            );
            None
        }
    }
}

fn record_ui_notify() {
    if import_debug() {
        if let Ok(mut n) = UI_NOTIFY_COUNT.get_or_init(|| Mutex::new(0)).lock() {
            *n += 1;
        }
    }
}

fn maybe_log_notify_count() {
    if !import_debug() {
        return;
    }
    if let Ok(n) = UI_NOTIFY_COUNT.get_or_init(|| Mutex::new(0)).lock() {
        eprintln!("[audio-import] ui_notify_count={n}");
    }
}

/// Install peak chunks on the async executor with throttled UI refresh (WebUI-style progressive draw).
fn install_preview_chunks_progressive(
    path_key: &str,
    preview: Arc<WaveformPreview>,
    timeline: &WeakEntity<Timeline>,
    cx: &mut AsyncApp,
) {
    let lod = preview
        .lods
        .iter()
        .find(|l| l.samples_per_peak == PEAK_FINE_SPP)
        .or_else(|| preview.lods.first());
    let Some(lod) = lod else {
        waveform_cache::finish_peak_build(path_key, preview);
        return;
    };
    let meta = Arc::new(WaveformFileMeta {
        sample_rate: preview.sample_rate,
        channels: preview.channels,
        duration_seconds: preview.duration_seconds,
        total_frames: preview.total_frames,
        peak_count: lod.peaks.len(),
        primary_spp: lod.samples_per_peak,
    });
    let chunks_total = lod.peaks.len().div_ceil(CHUNK_PEAKS);
    waveform_cache::begin_peak_build(path_key, Arc::clone(&meta), chunks_total);
    waveform_cache::set_import_state(
        path_key,
        AudioImportState::GeneratingPeaks { progress: 0.0 },
    );
    throttled_timeline_notify(timeline, cx, true);

    let spp = lod.samples_per_peak as u32;
    for chunk_index in 0..chunks_total {
        let start = chunk_index * CHUNK_PEAKS;
        let end = (start + CHUNK_PEAKS).min(lod.peaks.len());
        let slice = Arc::new(lod.peaks[start..end].to_vec());
        waveform_cache::install_chunk(path_key, spp, chunk_index as u32, slice);
        if chunk_index == 0 || chunk_index + 1 == chunks_total || chunk_index % 4 == 0 {
            let progress = (chunk_index + 1) as f32 / chunks_total as f32;
            waveform_cache::set_import_state(
                path_key,
                AudioImportState::GeneratingPeaks { progress },
            );
            throttled_timeline_notify(timeline, cx, chunk_index + 1 == chunks_total);
        }
    }

    // Install the remaining LOD chunks after the primary fine pass. This keeps
    // the first visible waveform quick while making zoomed-out renders read
    // from coarser chunk data instead of scanning excessive fine peaks.
    for other_lod in preview
        .lods
        .iter()
        .filter(|other_lod| other_lod.samples_per_peak != lod.samples_per_peak)
    {
        let spp = other_lod.samples_per_peak as u32;
        let lod_chunks_total = other_lod.peaks.len().div_ceil(CHUNK_PEAKS);
        for chunk_index in 0..lod_chunks_total {
            let start = chunk_index * CHUNK_PEAKS;
            let end = (start + CHUNK_PEAKS).min(other_lod.peaks.len());
            let slice = Arc::new(other_lod.peaks[start..end].to_vec());
            waveform_cache::install_chunk(path_key, spp, chunk_index as u32, slice);
        }
    }
    waveform_cache::finish_peak_build(path_key, preview);
}

/// Throttled UI refresh: ≤10 Hz unless `force` (state transition).
pub fn throttled_timeline_notify(timeline: &WeakEntity<Timeline>, cx: &mut AsyncApp, force: bool) {
    let throttle =
        NOTIFY_THROTTLE.get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(1)));
    let mut last = throttle.lock().expect("notify throttle");
    if !force && last.elapsed() < Duration::from_millis(100) {
        return;
    }
    *last = Instant::now();
    drop(last);

    let _ = timeline.update(cx, |_, cx| {
        record_ui_notify();
        cx.notify();
    });
}

fn run_peak_job(
    path: &Path,
    asset_id: &str,
    project_root: Option<&Path>,
) -> Result<Arc<WaveformPreview>, String> {
    let started = Instant::now();
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "[WaveformCache] generate started asset_id={asset_id} path={} size={file_size}",
        path.display()
    );

    if let Some(root) = project_root {
        if let Some(preview) = try_load_project_peak_cache(root, asset_id, path) {
            eprintln!("[WaveformCache] memory hit asset_id={asset_id} (disk preload)");
            waveform_cache::ingest_preview_as_chunks(asset_id, Arc::clone(&preview));
            return Ok(preview);
        }
    }

    waveform_cache::set_import_state(
        asset_id,
        AudioImportState::GeneratingPeaks { progress: 0.0 },
    );

    let peaks = DirectAudio::generate_audio_peaks(path).map_err(|e| e.to_string())?;
    let preview: WaveformPreview = peaks.into();
    let preview = Arc::new(preview);

    if let Some(root) = project_root {
        let peak_relative = waveform_peak_relative_path_for_asset(asset_id);
        eprintln!(
            "[AudioImport] project cache path={}",
            project_peak_path(root, asset_id).display()
        );
        let source = SourceFingerprint::for_path(path);
        save_project_peak_cache(root, asset_id, preview.as_ref(), source);
        eprintln!("[AudioImport] project asset path={}", path.display());
        let _ = peak_relative;
    }

    if import_debug() {
        let total_peaks: usize = preview.lods.iter().map(|l| l.peaks.len()).sum();
        eprintln!(
            "[audio-import] peak cache completed asset_id={asset_id} scan_ms={} total_peaks={}",
            started.elapsed().as_millis(),
            total_peaks
        );
        maybe_log_notify_count();
    }

    Ok(preview)
}

/// Re-bind a freshly-dropped clip to an already-imported source's shared peaks.
///
/// Called when [`run_import_pipeline`] short-circuits on a cache hit. Pushes the
/// cached metadata (so the new clip gets the correct duration instead of the
/// placeholder) and flips its import state to `Ready`, then notifies the
/// timeline. No decode/peak work runs — the peaks are reused as-is.
fn rebind_cached_asset(
    key: &str,
    timeline: &WeakEntity<Timeline>,
    layout: &Option<WeakEntity<StudioLayout>>,
    cx: &mut AsyncApp,
) {
    let Some(preview) = waveform_cache::get_preview_arc(key) else {
        // No finished preview yet → an import is genuinely still running and will
        // bind this clip itself. Nothing to do.
        return;
    };
    eprintln!(
        "[AudioImport] cache hit key={key} sr={} ch={} duration={:.3}s — reusing shared peaks",
        preview.sample_rate, preview.channels, preview.duration_seconds
    );
    let path_key = key.to_string();
    let layout_weak = layout.clone();
    let changed = timeline
        .update(cx, move |timeline, cx| {
            let changed = timeline.state.update_audio_clip_metadata(
                &path_key,
                "cached",
                preview.sample_rate,
                preview.channels,
                preview.total_frames,
                preview.duration_seconds,
            );
            timeline
                .state
                .set_audio_import_for_asset(&path_key, AudioImportState::Ready);
            if changed {
                eprintln!("[AudioImport] cache hit clip metadata rebound path={path_key}");
            }
            cx.notify();
            changed
        })
        .unwrap_or(false);
    // Re-sync the engine OUTSIDE the timeline lease: `schedule_audio_project_sync`
    // reads `self.timeline`, so calling it inside `timeline.update` double-leases
    // the Timeline entity and panics.
    if changed {
        if let Some(owner) = layout_weak.as_ref() {
            let _ = owner.update(cx, |this, cx| {
                this.mark_engine_media_dirty();
                this.schedule_audio_project_sync(cx, false, "audio_import_cache_hit");
            });
        }
    }
    throttled_timeline_notify(timeline, cx, true);
}

/// Opt-out kill switch for Phase D eager copy-into-project.
fn eager_copy_disabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_DISABLE_EAGER_AUDIO_COPY").is_some())
}

/// Phase D: if the project is saved and the dropped file lives outside its
/// folder, copy it into `Assets/Audio` (deduped) on a background thread and
/// retarget every clip sharing `asset_key` to the project-local copy. Only when
/// a file was actually copied does the asset id (`file_id`) move to the copy's
/// project-relative path, with the waveform cache migrated to the new key; that
/// happens right after a drop, before the id was ever saved. A source that
/// already lives in the project (every reopened clip) keeps its id: it is the
/// ARA audio-source persistentID and the peak-cache key, and must survive a
/// reopen. Returns the path to actually decode (the copy when copied, otherwise
/// the original) and the key to import under. Falls back to the original on any
/// error so a failed copy never breaks the clip.
async fn maybe_copy_into_project(
    asset_key: &str,
    path: PathBuf,
    project_root: Option<PathBuf>,
    timeline: &WeakEntity<Timeline>,
    cx: &mut AsyncApp,
) -> (PathBuf, String) {
    if eager_copy_disabled() {
        return (path, asset_key.to_string());
    }
    let Some(root) = project_root else {
        return (path, asset_key.to_string());
    };

    let src = path.clone();
    let root_for_job = root.clone();
    let copied = cx
        .background_executor()
        .spawn(async move { crate::project::import_audio_file_to_project(&src, &root_for_job) })
        .await;

    match copied {
        Ok(dest) => {
            let dest_str = dest.to_string_lossy().to_string();
            let old_key = asset_key.to_string();
            let new_key = if dest != path {
                relative_path_in_project(&dest, &root).unwrap_or_else(|| old_key.clone())
            } else {
                old_key.clone()
            };
            let _ = timeline.update(cx, |timeline, cx| {
                let mut changed = false;
                if dest_str != path.to_string_lossy() {
                    changed |= timeline.state.retarget_audio_source(&old_key, &dest_str);
                }
                if new_key != old_key {
                    changed |= timeline.state.retarget_audio_asset_id(&old_key, &new_key);
                    waveform_cache::migrate_cache_key(&old_key, &new_key);
                }
                if changed {
                    timeline.mark_media_changed(cx);
                    timeline.mark_project_changed(cx);
                    cx.notify();
                }
            });
            if dest != path {
                eprintln!(
                    "[AudioImport] eager copy retargeted asset_id={new_key} dest={}",
                    dest.display()
                );
                eprintln!("[AudioImport] project asset path={}", dest.display());
            }
            (dest, new_key)
        }
        Err(error) => {
            eprintln!(
                "[AudioImport] eager copy failed asset_id={asset_key} error={error}; using original source"
            );
            (path, asset_key.to_string())
        }
    }
}

/// Idempotent: one background job per audio asset. `asset_key` is the clip's
/// stable `file_id` (the waveform-cache + import-state key); `path` is the file
/// to decode (the project-local copy once Phase D copies it in). They start
/// equal but are kept separate so a `source_path` rewrite never changes the key.
pub async fn run_import_pipeline(
    asset_key: String,
    path: PathBuf,
    project_root: Option<PathBuf>,
    timeline: WeakEntity<Timeline>,
    layout: Option<WeakEntity<StudioLayout>>,
    cx: &mut AsyncApp,
) {
    let mut key = asset_key;
    // Phase D: copy the dropped file into the project folder before importing,
    // and decode the copy. Retarget asset id to project-relative path when copied.
    let (path, copied_key) =
        maybe_copy_into_project(&key, path, project_root.clone(), &timeline, cx).await;
    key = copied_key;
    if !waveform_cache::try_begin_import(&key) {
        // Already imported, or an import is still in flight for this source path.
        //
        // Repeated drag of the same file lands here: a fresh clip referencing
        // `key` was just inserted (fallback duration, `Pending` import) but the
        // peak job will not run a second time. If a finished preview already
        // exists in the shared cache, re-bind its metadata + `Ready` state onto
        // the new clip so the waveform renders from the shared peaks instead of
        // being stuck at the placeholder length. If an import is still running,
        // its own `update_audio_clip_metadata`/`set_audio_import_for_asset` calls
        // match by asset key and already cover the freshly-dropped clip.
        rebind_cached_asset(&key, &timeline, &layout, cx);
        return;
    }

    let path_for_job = path.clone();
    let timeline_probe = timeline.clone();
    let timeline_peaks = timeline.clone();
    let layout_weak = layout.clone();

    // ── Probe metadata ───────────────────────────────────────────────
    waveform_cache::set_import_state(&key, AudioImportState::Probing);
    throttled_timeline_notify(&timeline_probe, cx, true);

    let meta_path = path_for_job.clone();
    let probe = cx
        .background_executor()
        .spawn(async move { DirectAudio::probe_audio_file(&meta_path) })
        .await;

    match probe {
        Ok(info) => {
            eprintln!(
                "[audio-import] metadata read path={} sr={} ch={} frames={} duration={:.3}s size={}",
                key,
                info.sample_rate,
                info.channels,
                info.total_frames,
                info.duration_seconds,
                std::fs::metadata(&path_for_job)
                    .map(|m| m.len())
                    .unwrap_or(0)
            );
            let format = info.format.as_str().to_string();
            let path_key = key.clone();
            let changed = timeline_probe
                .update(cx, move |timeline, _cx| {
                    let changed = timeline.state.update_audio_clip_metadata(
                        &path_key,
                        &format,
                        info.sample_rate,
                        info.channels,
                        info.total_frames,
                        info.duration_seconds,
                    );
                    timeline.state.set_audio_import_for_asset(
                        &path_key,
                        AudioImportState::Decoding { progress: 0.0 },
                    );
                    if changed {
                        eprintln!("[audio-import] clip metadata updated path={path_key}");
                    }
                    changed
                })
                .unwrap_or(false);
            // Engine re-sync OUTSIDE the timeline lease: `schedule_audio_project_sync`
            // reads `self.timeline`, so calling it inside `timeline.update`
            // double-leases the Timeline entity and panics.
            if changed {
                if let Some(owner) = layout_weak.as_ref() {
                    let _ = owner.update(cx, |this, cx| {
                        this.mark_engine_media_dirty();
                        this.schedule_audio_project_sync(cx, false, "audio_import_probe");
                    });
                }
            }
            throttled_timeline_notify(&timeline_probe, cx, true);
        }
        Err(error) => {
            eprintln!(
                "[audio-import] metadata read failed path={} error={}",
                key, error
            );
            waveform_cache::install_failed(&key, error.to_string());
            let path_key = key.clone();
            let _ = timeline_probe.update(cx, move |timeline, cx| {
                timeline.state.set_audio_import_for_asset(
                    &path_key,
                    AudioImportState::Failed {
                        message: "metadata read failed".to_string(),
                    },
                );
                cx.notify();
            });
            throttled_timeline_notify(&timeline_probe, cx, true);
            return;
        }
    }

    // ── Peak generation (streaming for WAV, off UI thread) ─────────────
    eprintln!("[audio-import] peak cache started path={key}");
    waveform_cache::set_import_state(&key, AudioImportState::GeneratingPeaks { progress: 0.0 });
    throttled_timeline_notify(&timeline_peaks, cx, true);

    let decode_path = path_for_job.clone();
    let path_key = key.clone();
    let path_key_for_job = path_key.clone();
    let project_root_for_job = project_root.clone();
    let result = cx
        .background_executor()
        .spawn(async move {
            run_peak_job(
                &decode_path,
                &path_key_for_job,
                project_root_for_job.as_deref(),
            )
        })
        .await;

    match result {
        Ok(preview) => {
            install_preview_chunks_progressive(&path_key, preview, &timeline_peaks, cx);
            let _ = timeline_peaks.update(cx, move |timeline, cx| {
                timeline
                    .state
                    .set_audio_import_for_asset(&path_key, AudioImportState::Ready);
                cx.notify();
            });
            throttled_timeline_notify(&timeline_peaks, cx, true);
        }
        Err(message) => {
            eprintln!("[audio-import] peak cache failed path={path_key} error={message}");
            waveform_cache::install_failed(&path_key, message.clone());
            let _ = timeline_peaks.update(cx, move |timeline, cx| {
                timeline.state.set_audio_import_for_asset(
                    &path_key,
                    AudioImportState::Failed {
                        message: message.clone(),
                    },
                );
                cx.notify();
            });
            throttled_timeline_notify(&timeline_peaks, cx, true);
        }
    }
}

/// Resolve an asset's on-disk audio path for waveform restore.
///
/// Native projects store a project-relative path; DAW imports (Cubase XML,
/// etc.) store an absolute media path and leave `relative_path` empty —
/// both must resolve, otherwise clips stay stuck on `Queued`.
fn resolve_asset_audio_path(
    asset: &crate::project::ProjectAsset,
    project_root: &Path,
) -> Option<PathBuf> {
    if let Some(rel) = asset.relative_path.as_ref() {
        Some(project_root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)))
    } else {
        asset.absolute_path.clone()
    }
}

/// Collect `(asset_id, optional audio path)` for every audio asset / clip.
///
/// When an asset is registered without a path (import with only
/// `absolute_path` later filled from the clip), clip `source_path` fills the
/// gap rather than being skipped.
pub(crate) fn collect_waveform_restore_entries(
    project: &crate::project::FutureboardProject,
    project_root: &Path,
) -> Vec<(String, Option<PathBuf>)> {
    use crate::project::ClipSource;

    let mut entries: Vec<(String, Option<PathBuf>)> = Vec::new();

    for asset in &project.assets {
        entries.push((
            asset.id.clone(),
            resolve_asset_audio_path(asset, project_root),
        ));
    }

    for track in &project.tracks {
        for clip in &track.clips {
            let (asset_id, source_path) = match &clip.source {
                ClipSource::Audio {
                    asset_id,
                    source_path,
                } => (asset_id, source_path.clone()),
                ClipSource::Rauf {
                    asset_id,
                    source_path,
                    ..
                } => (asset_id, Some(source_path.clone())),
                _ => continue,
            };
            if let Some((_, path)) = entries.iter_mut().find(|(id, _)| id == asset_id) {
                if path.is_none() {
                    *path = source_path;
                }
                continue;
            }
            entries.push((asset_id.clone(), source_path));
        }
    }

    entries
}

/// After a project load, hydrate waveform caches from `Cache/Waveforms/` and
/// schedule background regeneration for missing peak files.
///
/// `persist_peaks` is true for saved Futureboard projects (read/write peak
/// files under the project folder and eager-copy media). Foreign imports that
/// bind as untitled pass `false` so media next to a Cubase XML is never copied
/// or peak-cached into the archive folder — waveforms are built in memory.
pub fn schedule_project_waveform_restore(
    project: &crate::project::FutureboardProject,
    project_root: PathBuf,
    persist_peaks: bool,
    timeline: Entity<Timeline>,
    layout: Entity<StudioLayout>,
    cx: &mut Context<StudioLayout>,
) {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut jobs: Vec<(String, PathBuf)> = Vec::new();
    let mut engine_resync_needed = false;
    let entries = collect_waveform_restore_entries(project, &project_root);

    for (asset_id, audio_path) in entries {
        if !seen.insert(asset_id.clone()) {
            continue;
        }
        if let Some(path) = audio_path.as_ref().filter(|p| p.exists()) {
            eprintln!("[ProjectLoad] audio asset resolved path={}", path.display());
        }

        if !persist_peaks {
            // Untitled DAW import: no project cache folder to read or write.
            eprintln!("[WaveformCache] import session disk miss asset_id={asset_id}");
            if let Some(path) = audio_path.filter(|p| p.exists()) {
                jobs.push((asset_id, path));
            }
            continue;
        }

        let peak_rel = project
            .assets
            .iter()
            .find(|a| a.id == asset_id)
            .and_then(|a| a.waveform_peak_relative_path.clone())
            .unwrap_or_else(|| waveform_peak_relative_path_for_asset(&asset_id));
        let peak_path = project_root.join(peak_rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        eprintln!(
            "[ProjectLoad] peak cache resolved path={}",
            peak_path.display()
        );

        // Validate the cached peaks against the current source file's
        // (size, mtime) when the source is reachable; a missing source stays
        // lenient so we keep showing cached peaks for moved/offline media.
        let expected_source = audio_path
            .as_ref()
            .and_then(|p| SourceFingerprint::for_path(p));
        match read_peak_file(&peak_path, Some(&asset_id), expected_source) {
            Ok(preview) => {
                let peak_count: usize = preview.lods.iter().map(|l| l.peaks.len()).sum();
                eprintln!(
                    "[WaveformCache] loaded path={} peaks={peak_count}",
                    peak_path.display()
                );
                let preview = Arc::new(preview);
                let format = (
                    preview.sample_rate,
                    preview.channels,
                    preview.total_frames,
                    preview.duration_seconds,
                );
                waveform_cache::ingest_preview_as_chunks(&asset_id, preview);
                waveform_cache::set_import_state(&asset_id, AudioImportState::Ready);
                let asset_id_for_timeline = asset_id.clone();
                let changed = timeline.update(cx, |timeline, cx| {
                    // No decode runs on a cache hit, so this is the only place
                    // the reopened clips learn their source's format (the
                    // Inspector's Source Duration). Metadata only: a saved
                    // clip's window and length are what the user left.
                    let (sample_rate, channels, total_frames, duration_seconds) = format;
                    let changed = timeline.state.apply_cached_audio_source_format(
                        &asset_id_for_timeline,
                        sample_rate,
                        channels,
                        total_frames,
                        duration_seconds,
                    );
                    timeline.state.set_audio_import_for_asset(
                        &asset_id_for_timeline,
                        AudioImportState::Ready,
                    );
                    cx.notify();
                    changed
                });
                engine_resync_needed |= changed;
            }
            Err(PeakFileError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("[WaveformCache] disk miss asset_id={asset_id}");
                if let Some(path) = audio_path.filter(|p| p.exists()) {
                    jobs.push((asset_id, path));
                }
            }
            Err(PeakFileError::SourceChanged { .. }) => {
                eprintln!("[WaveformCache] source changed; regenerating asset_id={asset_id}");
                if let Some(path) = audio_path.filter(|p| p.exists()) {
                    jobs.push((asset_id, path));
                }
            }
            Err(err) => {
                eprintln!(
                    "[WaveformCache] corrupt peak file; regenerating path={} error={err}",
                    peak_path.display()
                );
                if let Some(path) = audio_path.filter(|p| p.exists()) {
                    jobs.push((asset_id, path));
                }
            }
        }
    }

    if engine_resync_needed {
        // A clip decoded for the first time had its window converted to the
        // file's rate. Re-sync outside this StudioLayout update: updating the
        // layout entity from inside its own update would double-lease it.
        let layout_for_sync = layout.clone();
        cx.defer(move |cx| {
            let _ = layout_for_sync.update(cx, |this, cx| {
                this.mark_engine_media_dirty();
                this.schedule_audio_project_sync(cx, false, "waveform_cache_format");
            });
        });
    }

    if jobs.is_empty() {
        return;
    }

    let timeline_weak = timeline.downgrade();
    let layout_weak = layout.downgrade();
    // Only a real Futureboard project folder may receive peak files / copies.
    let persist_root = persist_peaks.then_some(project_root);
    cx.spawn(async move |_layout, cx| {
        for (asset_id, path) in jobs {
            run_import_pipeline(
                asset_id,
                path,
                persist_root.clone(),
                timeline_weak.clone(),
                Some(layout_weak.clone()),
                cx,
            )
            .await;
        }
    })
    .detach();
}

/// Timeline drop import entry point.
///
/// Must be called from inside `Timeline`'s own `update` (e.g. file-drop handler).
/// Do not call `timeline.update` here — the caller already holds the entity lease.
/// Clip `audio_import` is set to `Pending` in `insert_audio_clip`.
pub fn spawn_timeline_import(
    path: PathBuf,
    project_root: Option<PathBuf>,
    _timeline: Entity<Timeline>,
    layout: Option<Entity<StudioLayout>>,
    cx: &mut Context<Timeline>,
) {
    // The dropped clip's `file_id` is its `source_path` string at creation, so
    // the asset key is derived from the same path here.
    let asset_key = path.to_string_lossy().to_string();
    waveform_cache::request_decode_file(path.clone());

    let timeline_weak = _timeline.downgrade();
    let layout_weak = layout.map(|e| e.downgrade());
    cx.spawn(async move |_timeline, cx| {
        run_import_pipeline(
            asset_key,
            path,
            project_root,
            timeline_weak,
            layout_weak,
            cx,
        )
        .await;
    })
    .detach();
}

/// Browser / layout import entry (StudioLayout context).
pub fn spawn_timeline_import_from_layout(
    path: PathBuf,
    project_root: Option<PathBuf>,
    timeline: Entity<Timeline>,
    layout: Entity<StudioLayout>,
    cx: &mut Context<StudioLayout>,
) {
    let asset_key = path.to_string_lossy().to_string();
    let path_key = asset_key.clone();
    let _ = timeline.update(cx, |timeline, _cx| {
        timeline
            .state
            .set_audio_import_for_asset(&path_key, AudioImportState::Pending);
    });
    waveform_cache::request_decode_file(path.clone());

    let timeline_weak = timeline.downgrade();
    let layout_weak = layout.downgrade();
    cx.spawn(async move |_layout, cx| {
        run_import_pipeline(
            asset_key,
            path,
            project_root,
            timeline_weak,
            Some(layout_weak),
            cx,
        )
        .await;
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{
        ClipSource, FutureboardProject, ProjectAsset, ProjectClip, ProjectTrack, ProjectTrackType,
    };

    fn audio_asset(id: &str, absolute: Option<PathBuf>, relative: Option<&str>) -> ProjectAsset {
        ProjectAsset {
            id: id.to_string(),
            original_filename: "kick.wav".into(),
            relative_path: relative.map(str::to_string),
            absolute_path: absolute,
            duration_secs: Some(1.0),
            sample_rate: Some(44_100),
            channels: Some(2),
            source_fingerprint: None,
            waveform_peak_relative_path: None,
            duration_samples: Some(44_100),
        }
    }

    fn audio_clip(asset_id: &str, source: Option<PathBuf>) -> ProjectClip {
        ProjectClip {
            id: "clip".into(),
            name: "clip".into(),
            start_beat: 0.0,
            duration_beats: 4.0,
            offset_beats: 0.0,
            gain: 1.0,
            muted: false,
            source: ClipSource::Audio {
                asset_id: asset_id.into(),
                source_path: source,
            },
            stretch: Default::default(),
        }
    }

    #[test]
    fn restore_entries_use_absolute_path_when_relative_is_absent() {
        // Cubase XML import stores absolute media paths and no project-relative
        // copy — the resolve step must still surface the file so clips leave
        // `Queued`.
        let media = PathBuf::from("/tmp/cubase_archive/Audio/kick.wav");
        let mut project = FutureboardProject::new("imported");
        project
            .assets
            .push(audio_asset("a1", Some(media.clone()), None));
        project.tracks.push(ProjectTrack {
            id: "t1".into(),
            name: "KICK".into(),
            track_type: ProjectTrackType::Audio,
            ara: None,
            timebase: crate::components::timeline::timeline_state::TrackTimebase::Musical.to_tag(),
            parent_group_id: None,
            group_collapsed: false,
            color_hex: "#000000".into(),
            volume_norm: 0.75,
            pan: 0.0,
            muted: false,
            solo: false,
            record_arm: false,
            input_monitor: Default::default(),
            routing: Default::default(),
            inserts: Vec::new(),
            automation_lanes: Vec::new(),
            clips: vec![audio_clip("a1", Some(media.clone()))],
            row_height_px: None,
            soundfont: None,
            volume_automation_read: true,
            solfege: None,
            takes: Vec::new(),
            takes_expanded: false,
        });

        let entries = collect_waveform_restore_entries(&project, Path::new("/tmp/unused"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "a1");
        assert_eq!(entries[0].1.as_deref(), Some(media.as_path()));
    }

    #[test]
    fn restore_entries_prefer_relative_path_for_native_projects() {
        let mut project = FutureboardProject::new("native");
        project.assets.push(audio_asset(
            "a1",
            Some(PathBuf::from("/elsewhere/kick.wav")),
            Some("Assets/Audio/kick.wav"),
        ));
        project.tracks.push(ProjectTrack {
            id: "t1".into(),
            name: "KICK".into(),
            track_type: ProjectTrackType::Audio,
            ara: None,
            timebase: crate::components::timeline::timeline_state::TrackTimebase::Musical.to_tag(),
            parent_group_id: None,
            group_collapsed: false,
            color_hex: "#000000".into(),
            volume_norm: 0.75,
            pan: 0.0,
            muted: false,
            solo: false,
            record_arm: false,
            input_monitor: Default::default(),
            routing: Default::default(),
            inserts: Vec::new(),
            automation_lanes: Vec::new(),
            clips: vec![audio_clip("a1", Some(PathBuf::from("/elsewhere/kick.wav")))],
            row_height_px: None,
            soundfont: None,
            volume_automation_read: true,
            solfege: None,
            takes: Vec::new(),
            takes_expanded: false,
        });

        let root = PathBuf::from("/proj");
        let entries = collect_waveform_restore_entries(&project, &root);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].1.as_deref(),
            Some(root.join("Assets/Audio/kick.wav").as_path())
        );
    }

    #[test]
    fn restore_entries_fill_path_from_clip_when_asset_has_neither() {
        let media = PathBuf::from("/media/kick.wav");
        let mut project = FutureboardProject::new("gap");
        project.assets.push(audio_asset("a1", None, None));
        project.tracks.push(ProjectTrack {
            id: "t1".into(),
            name: "KICK".into(),
            track_type: ProjectTrackType::Audio,
            ara: None,
            timebase: crate::components::timeline::timeline_state::TrackTimebase::Musical.to_tag(),
            parent_group_id: None,
            group_collapsed: false,
            color_hex: "#000000".into(),
            volume_norm: 0.75,
            pan: 0.0,
            muted: false,
            solo: false,
            record_arm: false,
            input_monitor: Default::default(),
            routing: Default::default(),
            inserts: Vec::new(),
            automation_lanes: Vec::new(),
            clips: vec![audio_clip("a1", Some(media.clone()))],
            row_height_px: None,
            soundfont: None,
            volume_automation_read: true,
            solfege: None,
            takes: Vec::new(),
            takes_expanded: false,
        });

        let entries = collect_waveform_restore_entries(&project, Path::new("/tmp"));
        assert_eq!(entries[0].1.as_deref(), Some(media.as_path()));
    }
}

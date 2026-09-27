//! Background audio import: probe → peak generation → disk cache.
//!
//! Never runs decode/peak work on the GPUI render thread. UI updates are
//! throttled to ≤10 Hz except on state transitions.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{AsyncApp, Context, Entity, WeakEntity};

use super::timeline::Timeline;
use super::timeline_state::{AudioImportState, ClipType, TimelineState};
use super::waveform_cache::{self, WaveformFileMeta, WaveformPreview, CHUNK_PEAKS, PEAK_FINE_SPP};
use super::waveform_peak_file::{
    legacy_waveform_peak_relative_path_for_asset, read_peak_file,
    waveform_peak_relative_path_for_asset, write_peak_file, PeakFileError, SourceFingerprint,
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

/// Peak files that may hold `asset_id`'s cache, in lookup order: today's name
/// (the only one ever written, so the freshest when it exists), the path the
/// project recorded for it (when that is a portable project-relative path;
/// an older save may have recorded an older name), then the name an earlier
/// build wrote.
fn peak_file_candidates(
    project_root: &Path,
    asset_id: &str,
    recorded: Option<&str>,
) -> Vec<PathBuf> {
    let mut relatives: Vec<String> = Vec::with_capacity(3);
    relatives.push(waveform_peak_relative_path_for_asset(asset_id));
    if let Some(recorded) = recorded.filter(|recorded| is_portable_peak_relative_path(recorded)) {
        relatives.push(recorded.to_string());
    }
    if let Some(legacy) = legacy_waveform_peak_relative_path_for_asset(asset_id) {
        relatives.push(legacy);
    }
    let mut paths: Vec<PathBuf> = Vec::with_capacity(relatives.len());
    for relative in relatives {
        let path = project_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// A recorded peak path that can be opened here as written: relative, no
/// `..`, no `:` (a stream name on NTFS), every component within the 255-byte
/// name limit. Paths recorded from ids before names were bounded can fail
/// all of these.
fn is_portable_peak_relative_path(relative: &str) -> bool {
    !relative.is_empty()
        && !relative.starts_with(['/', '\\'])
        && !relative.contains(':')
        && relative
            .split(['/', '\\'])
            .all(|component| component != ".." && component.len() <= 255)
}

/// Read the first candidate that holds usable peaks for `asset_id`. A
/// candidate that exists but is stale (its source changed) or damaged never
/// hides a later one that is current: an old name left behind must not
/// shadow a fresh cache. When none is usable, the first such error is
/// returned (the peaks are regenerated), or the first candidate's miss when
/// none could be opened at all.
fn read_first_peak_file(
    candidates: &[PathBuf],
    asset_id: &str,
    expected_source: Option<SourceFingerprint>,
) -> (PathBuf, Result<WaveformPreview, PeakFileError>) {
    let mut first_miss = None;
    let mut first_unusable = None;
    for path in candidates {
        match read_peak_file(path, Some(asset_id), expected_source) {
            Ok(preview) => return (path.clone(), Ok(preview)),
            Err(PeakFileError::Io(err)) => {
                first_miss.get_or_insert((path.clone(), PeakFileError::Io(err)));
            }
            Err(err) => {
                first_unusable.get_or_insert((path.clone(), err));
            }
        }
    }
    match first_unusable.or(first_miss) {
        Some((path, err)) => (path, Err(err)),
        None => (
            PathBuf::new(),
            Err(PeakFileError::Io(std::io::ErrorKind::NotFound.into())),
        ),
    }
}

fn try_load_project_peak_cache(
    project_root: &Path,
    asset_id: &str,
    source_path: &Path,
) -> Option<Arc<WaveformPreview>> {
    let expected_source = SourceFingerprint::for_path(source_path);
    let candidates = peak_file_candidates(project_root, asset_id, None);
    let (path, result) = read_first_peak_file(&candidates, asset_id, expected_source);
    match result {
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

/// Which clips an import may point at the project copy it makes of their file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportClips {
    /// Restoring a saved project: every clip sharing the asset is that saved
    /// asset. The copy becomes their source, but their ids are saved (the ARA
    /// audio-source persistentID, the peak-cache key) and never change.
    Saved,
    /// The clips this import just created. Only they move to the copy, source
    /// and id. Any other clip that shares the dropped path keeps both: its id
    /// may already be saved or bound to an ARA document, and the file at the
    /// dropped path may no longer hold its audio.
    Created(Vec<String>),
}

/// What pointing an import's clips at the project copy changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportRetarget {
    changed: bool,
    /// Key the import runs under: the copy's id when this import's clips moved
    /// to it, otherwise the dropped key.
    import_key: String,
    /// Every clip under the old key moved, so its cached peaks go with it.
    /// When some clip keeps the old key, its peaks stay where they are.
    move_cache: bool,
}

/// Point the clips `clips` allows at `copy_source`, and (for clips this import
/// created) at the copy's id `copy_key`. Clips sharing `old_key` that
/// `clips` does not name are left exactly as they are.
fn retarget_imported_clips(
    state: &mut TimelineState,
    clips: &ImportClips,
    old_key: &str,
    copy_key: &str,
    copy_source: &str,
) -> ImportRetarget {
    let mut changed = false;
    let mut moved_id = false;
    let mut kept_old_key = false;
    for clip in state
        .tracks
        .iter_mut()
        .flat_map(|track| track.clips.iter_mut())
    {
        if clip.audio_asset_key() != Some(old_key) {
            continue;
        }
        let may_move = match clips {
            ImportClips::Saved => true,
            ImportClips::Created(ids) => ids.contains(&clip.id),
        };
        let ClipType::Audio {
            file_id,
            source_path,
        } = &mut clip.clip_type
        else {
            continue;
        };
        if !may_move {
            kept_old_key = true;
            continue;
        }
        if source_path.as_deref() != Some(copy_source) {
            *source_path = Some(copy_source.to_string());
            changed = true;
        }
        if matches!(clips, ImportClips::Created(_)) && file_id.as_str() != copy_key {
            *file_id = copy_key.to_string();
            changed = true;
            moved_id = true;
        } else {
            kept_old_key = true;
        }
    }
    ImportRetarget {
        changed,
        import_key: if moved_id { copy_key } else { old_key }.to_string(),
        move_cache: moved_id && !kept_old_key,
    }
}

/// The clips a layout-side import just created for `asset_key`. Every such
/// caller inserts the clip through `insert_audio_clip_with_duration`, which
/// selects exactly that clip, right before starting its import; a new clip
/// also still points at the dropped file. A caller that selected nothing
/// matching moves no clip: ids stay as they are, which is always safe.
fn clips_just_created(state: &TimelineState, asset_key: &str) -> Vec<String> {
    state
        .tracks
        .iter()
        .flat_map(|track| track.clips.iter())
        .filter(|clip| {
            clip.audio_asset_key() == Some(asset_key)
                && state.selection.selected_clip_ids.contains(&clip.id)
                && matches!(
                    &clip.clip_type,
                    ClipType::Audio { source_path: Some(source), .. } if source == asset_key
                )
        })
        .map(|clip| clip.id.clone())
        .collect()
}

/// Show the clips a layout-side import is about to import as pending. With
/// clips just created, only those: they alone move to the project copy's key,
/// and every progress report after that is made under that key, so another
/// clip sharing the dropped path (a saved clip, maybe long since `Ready`)
/// would be left pending for good. With none, every clip under the key: the
/// import then runs under that key and reports to all of them.
fn mark_import_pending(state: &mut TimelineState, asset_key: &str, created: &[String]) {
    for clip in state
        .tracks
        .iter_mut()
        .flat_map(|track| track.clips.iter_mut())
    {
        if clip.audio_asset_key() == Some(asset_key)
            && (created.is_empty() || created.contains(&clip.id))
        {
            clip.audio_import = AudioImportState::Pending;
        }
    }
}

/// Phase D: if the project is saved and the dropped file lives outside its
/// folder, copy it into `Assets/Audio` (deduped) on a background thread and
/// point the clips `clips` allows at the project-local copy. Only clips this
/// import created take the copy's project-relative path as their asset id
/// (`file_id`), with the waveform cache migrated when no other clip still
/// uses the old key; that happens right after a drop, before the id was ever
/// saved. Every other clip keeps its id: it is the ARA audio-source
/// persistentID and the peak-cache key, and must survive a reopen. Returns the
/// path to actually decode (the copy when copied, otherwise the original) and
/// the key to import under. Falls back to the original on any error so a
/// failed copy never breaks the clip.
async fn maybe_copy_into_project(
    asset_key: &str,
    path: PathBuf,
    project_root: Option<PathBuf>,
    clips: &ImportClips,
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
            if dest == path {
                return (dest, asset_key.to_string());
            }
            let dest_str = dest.to_string_lossy().to_string();
            let old_key = asset_key.to_string();
            let copy_key =
                relative_path_in_project(&dest, &root).unwrap_or_else(|| old_key.clone());
            let retarget = timeline
                .update(cx, |timeline, cx| {
                    let retarget = retarget_imported_clips(
                        &mut timeline.state,
                        clips,
                        &old_key,
                        &copy_key,
                        &dest_str,
                    );
                    if retarget.move_cache {
                        waveform_cache::migrate_cache_key(&old_key, &copy_key);
                    }
                    if retarget.changed {
                        timeline.mark_media_changed(cx);
                        timeline.mark_project_changed(cx);
                        cx.notify();
                    }
                    retarget
                })
                .ok();
            let import_key = retarget.map_or(old_key, |retarget| retarget.import_key);
            eprintln!(
                "[AudioImport] eager copy retargeted asset_id={import_key} dest={}",
                dest.display()
            );
            eprintln!("[AudioImport] project asset path={}", dest.display());
            (dest, import_key)
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
    clips: ImportClips,
    timeline: WeakEntity<Timeline>,
    layout: Option<WeakEntity<StudioLayout>>,
    cx: &mut AsyncApp,
) {
    let mut key = asset_key;
    // Phase D: copy the dropped file into the project folder before importing,
    // and decode the copy. Only clips this import created take the copy's id.
    let (path, copied_key) =
        maybe_copy_into_project(&key, path, project_root.clone(), &clips, &timeline, cx).await;
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

        let recorded_peak_rel = project
            .assets
            .iter()
            .find(|a| a.id == asset_id)
            .and_then(|a| a.waveform_peak_relative_path.as_deref());
        let candidates = peak_file_candidates(&project_root, &asset_id, recorded_peak_rel);

        // Validate the cached peaks against the current source file's
        // (size, mtime) when the source is reachable; a missing source stays
        // lenient so we keep showing cached peaks for moved/offline media.
        let expected_source = audio_path
            .as_ref()
            .and_then(|p| SourceFingerprint::for_path(p));
        let (peak_path, peak_read) = read_first_peak_file(&candidates, &asset_id, expected_source);
        eprintln!(
            "[ProjectLoad] peak cache resolved path={}",
            peak_path.display()
        );
        match peak_read {
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
                ImportClips::Saved,
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
/// `created_clip_ids` are the clips the drop just inserted: only they may move
/// to the project copy's asset id.
pub fn spawn_timeline_import(
    path: PathBuf,
    project_root: Option<PathBuf>,
    created_clip_ids: Vec<String>,
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
            ImportClips::Created(created_clip_ids),
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
    let created_clip_ids = timeline.update(cx, |timeline, _cx| {
        let created = clips_just_created(&timeline.state, &path_key);
        mark_import_pending(&mut timeline.state, &path_key, &created);
        created
    });
    waveform_cache::request_decode_file(path.clone());

    let timeline_weak = timeline.downgrade();
    let layout_weak = layout.downgrade();
    cx.spawn(async move |_layout, cx| {
        run_import_pipeline(
            asset_key,
            path,
            project_root,
            ImportClips::Created(created_clip_ids),
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

    fn clip_audio<'a>(state: &'a TimelineState, clip_id: &str) -> (&'a str, Option<&'a str>) {
        let clip = state
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .find(|clip| clip.id == clip_id)
            .expect("clip");
        match &clip.clip_type {
            ClipType::Audio {
                file_id,
                source_path,
            } => (file_id.as_str(), source_path.as_deref()),
            other => panic!("expected an audio clip, got {other:?}"),
        }
    }

    /// A saved clip whose id is still its absolute drop path (saved from an
    /// untitled session, maybe bound to Melodyne) and a fresh drop of the same
    /// file. The eager copy used to re-identify both, which changed the saved
    /// clip's ARA persistentID.
    fn saved_and_redropped() -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        let dropped = "/Samples/vocal.wav".to_string();
        let saved = state.import_audio_to_selected_or_new_track(dropped.clone(), "vocal".into());
        // Its save copied the media into the project and pointed it there.
        let _ = state.retarget_audio_source(&dropped, "/Song/Assets/Audio/vocal.wav");
        let fresh = state.import_audio_to_selected_or_new_track(dropped, "vocal".into());
        (state, saved, fresh)
    }

    #[test]
    fn a_redrop_moves_only_the_new_clip_to_the_copy() {
        let (mut state, saved, fresh) = saved_and_redropped();
        assert_eq!(
            clips_just_created(&state, "/Samples/vocal.wav"),
            vec![fresh.clone()]
        );

        let retarget = retarget_imported_clips(
            &mut state,
            &ImportClips::Created(vec![fresh.clone()]),
            "/Samples/vocal.wav",
            "Assets/Audio/vocal-1.wav",
            "/Song/Assets/Audio/vocal-1.wav",
        );
        assert_eq!(
            clip_audio(&state, &saved),
            ("/Samples/vocal.wav", Some("/Song/Assets/Audio/vocal.wav")),
            "the saved clip keeps its id and its own media"
        );
        assert_eq!(
            clip_audio(&state, &fresh),
            (
                "Assets/Audio/vocal-1.wav",
                Some("/Song/Assets/Audio/vocal-1.wav")
            )
        );
        assert!(retarget.changed);
        assert_eq!(retarget.import_key, "Assets/Audio/vocal-1.wav");
        assert!(
            !retarget.move_cache,
            "the saved clip still reads its peaks under the old key"
        );
    }

    #[test]
    fn a_first_drop_moves_its_clip_and_its_cached_peaks() {
        let mut state = TimelineState::default();
        let fresh =
            state.import_audio_to_selected_or_new_track("/Samples/kick.wav".into(), "kick".into());
        let created = clips_just_created(&state, "/Samples/kick.wav");
        assert_eq!(created, vec![fresh.clone()]);
        let retarget = retarget_imported_clips(
            &mut state,
            &ImportClips::Created(created),
            "/Samples/kick.wav",
            "Assets/Audio/kick.wav",
            "/Song/Assets/Audio/kick.wav",
        );
        assert_eq!(
            clip_audio(&state, &fresh),
            ("Assets/Audio/kick.wav", Some("/Song/Assets/Audio/kick.wav"))
        );
        assert_eq!(retarget.import_key, "Assets/Audio/kick.wav");
        assert!(retarget.move_cache);
    }

    /// Restoring a saved project copies external media in but never changes a
    /// saved id.
    #[test]
    fn restoring_saved_clips_moves_their_media_but_never_their_ids() {
        let (mut state, saved, fresh) = saved_and_redropped();
        let retarget = retarget_imported_clips(
            &mut state,
            &ImportClips::Saved,
            "/Samples/vocal.wav",
            "Assets/Audio/vocal-1.wav",
            "/Song/Assets/Audio/vocal-1.wav",
        );
        for clip in [&saved, &fresh] {
            assert_eq!(clip_audio(&state, clip).0, "/Samples/vocal.wav");
            assert_eq!(
                clip_audio(&state, clip).1,
                Some("/Song/Assets/Audio/vocal-1.wav")
            );
        }
        assert_eq!(retarget.import_key, "/Samples/vocal.wav");
        assert!(!retarget.move_cache);
    }

    /// A layout caller that selected nothing it created moves nothing.
    #[test]
    fn without_a_created_clip_nothing_is_re_identified() {
        let (mut state, saved, _fresh) = saved_and_redropped();
        state.selection.selected_clip_ids.clear();
        let created = clips_just_created(&state, "/Samples/vocal.wav");
        assert!(created.is_empty());
        let retarget = retarget_imported_clips(
            &mut state,
            &ImportClips::Created(created),
            "/Samples/vocal.wav",
            "Assets/Audio/vocal-1.wav",
            "/Song/Assets/Audio/vocal-1.wav",
        );
        assert!(!retarget.changed);
        assert_eq!(retarget.import_key, "/Samples/vocal.wav");
        assert_eq!(clip_audio(&state, &saved).0, "/Samples/vocal.wav");
    }

    #[test]
    fn peak_lookup_tries_todays_name_then_the_recorded_path_then_the_old_one() {
        let root = Path::new("/p");
        let id = "/Users/me/Library/Mobile Documents/com~apple~CloudDocs/kick.wav";
        let candidates = peak_file_candidates(root, id, Some("Cache/Waveforms/recorded.peaks"));
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates[0],
            root.join(waveform_peak_relative_path_for_asset(id))
        );
        assert!(candidates[1].ends_with("recorded.peaks"));
        assert!(candidates[2]
            .to_string_lossy()
            .ends_with("com~apple~CloudDocs__kick.wav.peaks"));

        // An old save recorded the old name: looked up once, after today's.
        let legacy = legacy_waveform_peak_relative_path_for_asset(id).unwrap();
        let recorded_legacy = peak_file_candidates(root, id, Some(&legacy));
        assert_eq!(recorded_legacy.len(), 2);
        assert_eq!(
            recorded_legacy[0],
            root.join(waveform_peak_relative_path_for_asset(id))
        );

        // A recorded path that cannot be opened as written is skipped.
        let windows =
            peak_file_candidates(root, "C:\\a.wav", Some("Cache/Waveforms/C:__a.wav.peaks"));
        assert_eq!(windows.len(), 1);
        assert!(!is_portable_peak_relative_path(&format!(
            "Cache/Waveforms/{}.peaks",
            "x".repeat(300)
        )));
        assert!(!is_portable_peak_relative_path("../outside.peaks"));
        assert!(is_portable_peak_relative_path(
            "Cache/Waveforms/Assets__Audio__kick.wav.peaks"
        ));
    }

    fn one_peak_preview(max: f32) -> WaveformPreview {
        WaveformPreview {
            sample_rate: 48_000,
            channels: 1,
            duration_seconds: 1.0,
            total_frames: 48_000,
            lods: vec![waveform_cache::WaveformLod {
                samples_per_peak: 256,
                peaks: vec![waveform_cache::WaveformPeak { min: -max, max }],
            }],
        }
    }

    /// An old-name cache built for an earlier version of the source (the
    /// project recorded that name) must not hide the fresh cache under
    /// today's name, which would re-decode the file on every open.
    #[test]
    fn a_stale_old_name_never_shadows_a_fresh_cache() {
        let root = std::env::temp_dir().join(format!(
            "fb_peak_lookup_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let id = "/Users/me/Library/Mobile Documents/com~apple~CloudDocs/kick.wav";
        let legacy = legacy_waveform_peak_relative_path_for_asset(id).unwrap();
        let source_then = SourceFingerprint {
            size: 100,
            modified_nanos: 1,
        };
        let source_now = SourceFingerprint {
            size: 120,
            modified_nanos: 2,
        };
        let at = |relative: &str| root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        write_peak_file(&at(&legacy), id, &one_peak_preview(0.25), Some(source_then)).unwrap();
        let candidates = peak_file_candidates(&root, id, Some(&legacy));

        // Only the stale old file: reported stale, so the peaks regenerate.
        let (path, read) = read_first_peak_file(&candidates, id, Some(source_now));
        assert_eq!(path, at(&legacy));
        assert!(matches!(read, Err(PeakFileError::SourceChanged { .. })));

        // Regenerated under today's name: found first from then on.
        let today = waveform_peak_relative_path_for_asset(id);
        write_peak_file(&at(&today), id, &one_peak_preview(0.75), Some(source_now)).unwrap();
        let (path, read) = read_first_peak_file(&candidates, id, Some(source_now));
        assert_eq!(path, at(&today));
        assert_eq!(read.unwrap().lods[0].peaks[0].max, 0.75);

        // A stale file under today's name still lets a current old one load.
        write_peak_file(&at(&today), id, &one_peak_preview(0.75), Some(source_then)).unwrap();
        write_peak_file(&at(&legacy), id, &one_peak_preview(0.25), Some(source_now)).unwrap();
        let (path, read) = read_first_peak_file(&candidates, id, Some(source_now));
        assert_eq!(path, at(&legacy));
        assert!(read.is_ok());

        let _ = std::fs::remove_dir_all(root);
    }

    /// A layout-side re-import used to mark every clip sharing the dropped
    /// path pending, but only the new clip moves to the copy's key and hears
    /// about the import after that: the saved clip stayed "Importing…".
    #[test]
    fn a_layout_redrop_leaves_the_saved_clip_as_it_was() {
        let (mut state, saved, fresh) = saved_and_redropped();
        let import_state = |state: &TimelineState, clip_id: &str| {
            state
                .tracks
                .iter()
                .flat_map(|track| track.clips.iter())
                .find(|clip| clip.id == clip_id)
                .map(|clip| clip.audio_import.clone())
                .expect("clip")
        };
        state.set_audio_import_for_asset("/Samples/vocal.wav", AudioImportState::Ready);

        let created = clips_just_created(&state, "/Samples/vocal.wav");
        mark_import_pending(&mut state, "/Samples/vocal.wav", &created);
        assert_eq!(import_state(&state, &saved), AudioImportState::Ready);
        assert_eq!(import_state(&state, &fresh), AudioImportState::Pending);

        // Nothing known to be new: the import runs under the dropped key and
        // reports to every clip under it, so all of them wait for it.
        state.set_audio_import_for_asset("/Samples/vocal.wav", AudioImportState::Ready);
        mark_import_pending(&mut state, "/Samples/vocal.wav", &[]);
        assert_eq!(import_state(&state, &saved), AudioImportState::Pending);
        assert_eq!(import_state(&state, &fresh), AudioImportState::Pending);
    }
}

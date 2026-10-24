//! UI-side waveform peak cache — render path is read-only.
//!
//! Decoding and peak generation run on background threads via [`super::audio_import`].

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::timeline_state::AudioImportState;
use DirectAudio::{generate_audio_peaks, AudioPeak as EnginePeak, AudioPeakFile, PEAK_LOD_LEVELS};

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct WaveformPeak {
    pub min: f32,
    pub max: f32,
}

impl From<EnginePeak> for WaveformPeak {
    fn from(p: EnginePeak) -> Self {
        Self {
            min: p.min,
            max: p.max,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WaveformLod {
    pub samples_per_peak: usize,
    pub peaks: Vec<WaveformPeak>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WaveformPreview {
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_seconds: f64,
    pub total_frames: u64,
    pub lods: Vec<WaveformLod>,
}

impl From<AudioPeakFile> for WaveformPreview {
    fn from(peaks: AudioPeakFile) -> Self {
        Self {
            sample_rate: peaks.sample_rate,
            channels: peaks.channels,
            duration_seconds: peaks.duration_seconds,
            total_frames: peaks.total_frames,
            lods: peaks
                .lods
                .into_iter()
                .map(|lod| WaveformLod {
                    samples_per_peak: lod.samples_per_peak as usize,
                    peaks: lod.peaks.into_iter().map(WaveformPeak::from).collect(),
                })
                .collect(),
        }
    }
}

/// Matches Electron/WebUI `WaveformStatus` + native partial progressive draw.
#[derive(Debug, Clone)]
pub enum WaveformDisplayStatus {
    Pending,
    Partial {
        meta: Arc<WaveformFileMeta>,
        chunks_ready: usize,
        chunks_total: usize,
    },
    Ready {
        meta: Arc<WaveformFileMeta>,
    },
    Error(String),
}

/// Legacy alias used during migration.
pub type WaveformStatus = WaveformDisplayStatus;

/// Per-file peak metadata (chunk bytes live in `FileEntry::chunks`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WaveformFileMeta {
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_seconds: f64,
    pub total_frames: u64,
    pub peak_count: usize,
    pub primary_spp: usize,
}

/// Peaks per chunk — mirrors WebUI `CHUNK_PEAKS` (`peakChunkCache.ts`).
pub const CHUNK_PEAKS: usize = 4096;
/// Finest LOD used for chunking — mirrors WebUI `PEAK_FINE_SPP`.
pub const PEAK_FINE_SPP: usize = 256;
pub const WAVEFORM_ALGORITHM_VERSION: u32 = 2;

pub const MAX_PEAKS_PER_COLUMN: usize = 16;

/// Stored peaks aimed at each pixel column.
///
/// Four, not one: one peak per column makes every column's min/max a single
/// stored pair, so the drawn envelope is whatever the LOD builder happened to
/// bracket rather than what the audio did under that pixel. Four is enough for
/// the column to be an envelope and still well inside [`MAX_PEAKS_PER_COLUMN`],
/// which is what the aggregation reads.
pub const PEAKS_PER_COLUMN: usize = 4;

pub(crate) struct FileEntry {
    import: AudioImportState,
    meta: Option<Arc<WaveformFileMeta>>,
    chunks_total: usize,
    chunks_ready: usize,
    /// `(samples_per_peak, chunk_index)` → peaks
    chunks: HashMap<(u32, u32), Arc<Vec<WaveformPeak>>>,
    /// Full LOD ladder for zoom selection once complete.
    preview: Option<Arc<WaveformPreview>>,
    /// Monotonic counter bumped whenever the peak data changes (new chunk,
    /// preview installed, rebuild started). The render-side geometry cache
    /// folds this into its key so cached bars are discarded when peaks change.
    revision: u64,
}

/// Revision of a locked entry — see [`FileEntry::revision`].
pub(crate) fn entry_revision(entry: &FileEntry) -> u64 {
    entry.revision
}

/// LOD levels — mirrors `DirectAudio::PEAK_LOD_LEVELS`.
pub const LOD_LEVELS: [usize; 9] = [256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];

const _: () = {
    assert!(LOD_LEVELS.len() == PEAK_LOD_LEVELS.len());
};

static CLIP_CACHE: OnceLock<Mutex<HashMap<String, Arc<WaveformPreview>>>> = OnceLock::new();
static FILE_CACHE: OnceLock<Mutex<HashMap<String, FileEntry>>> = OnceLock::new();

static TIMELINE_DEBUG: OnceLock<bool> = OnceLock::new();
static RENDER_STATS: OnceLock<Mutex<TimelineRenderStats>> = OnceLock::new();

#[derive(Default)]
struct TimelineRenderStats {
    visible_clips: u64,
    waveform_bars: u64,
    cache_hits: u64,
    cache_misses: u64,
    window_start: Option<std::time::Instant>,
}

fn timeline_debug() -> bool {
    *TIMELINE_DEBUG.get_or_init(|| std::env::var_os("FUTUREBOARD_TIMELINE_DEBUG").is_some())
}

pub fn record_timeline_render(visible_clips: usize, waveform_bars: usize, cache_hit: bool) {
    if crate::perf::enabled() {
        crate::perf::count("waveform_bars", waveform_bars as u64);
        crate::perf::count("visible_clips", visible_clips as u64);
        if cache_hit {
            crate::perf::count("waveform_cache_hit", 1);
        }
    }
    if !timeline_debug() {
        return;
    }
    let stats = RENDER_STATS.get_or_init(|| Mutex::new(TimelineRenderStats::default()));
    let mut s = stats.lock().expect("timeline render stats");
    if s.window_start.is_none() {
        s.window_start = Some(std::time::Instant::now());
    }
    s.visible_clips += visible_clips as u64;
    s.waveform_bars += waveform_bars as u64;
    if cache_hit {
        s.cache_hits += 1;
    } else {
        s.cache_misses += 1;
    }
    if let Some(start) = s.window_start {
        if start.elapsed() >= std::time::Duration::from_secs(1) {
            eprintln!(
                "[timeline-debug] visible_clips={} waveform_bars={} cache_hits={} cache_misses={}",
                s.visible_clips, s.waveform_bars, s.cache_hits, s.cache_misses
            );
            *s = TimelineRenderStats::default();
        }
    }
}

fn clip_cache() -> &'static Mutex<HashMap<String, Arc<WaveformPreview>>> {
    CLIP_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_cache() -> &'static Mutex<HashMap<String, FileEntry>> {
    FILE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Move an in-memory cache entry from `old_key` to `new_key` (e.g. after
/// retargeting an asset id to a project-relative path).
pub fn migrate_cache_key(old_key: &str, new_key: &str) {
    if old_key == new_key {
        return;
    }
    if let Ok(mut cache) = file_cache().lock() {
        if let Some(entry) = cache.remove(old_key) {
            eprintln!("[WaveformCache] migrate asset_id {old_key} -> {new_key}");
            if let Some(existing) = cache.get(new_key) {
                if existing.preview.is_some() || existing.chunks_ready > 0 {
                    return;
                }
            }
            cache.insert(new_key.to_string(), entry);
        }
    }
}

pub fn get_preview_arc(path: &str) -> Option<Arc<WaveformPreview>> {
    file_cache()
        .lock()
        .ok()?
        .get(path)
        .and_then(|e| e.preview.clone())
}

pub fn get_file_status(path: &str) -> WaveformDisplayStatus {
    let Ok(cache) = file_cache().lock() else {
        return WaveformDisplayStatus::Pending;
    };
    let Some(entry) = cache.get(path) else {
        return WaveformDisplayStatus::Pending;
    };
    if let (Some(meta), true) = (&entry.meta, entry.preview.is_some()) {
        return WaveformDisplayStatus::Ready {
            meta: Arc::clone(meta),
        };
    }
    if let Some(meta) = &entry.meta {
        if entry.chunks_ready > 0 {
            return WaveformDisplayStatus::Partial {
                meta: Arc::clone(meta),
                chunks_ready: entry.chunks_ready,
                chunks_total: entry.chunks_total.max(1),
            };
        }
    }
    match &entry.import {
        AudioImportState::Failed { message } => WaveformDisplayStatus::Error(message.clone()),
        _ => WaveformDisplayStatus::Pending,
    }
}

/// Read the immutable source metadata for an already-known asset without
/// exposing the cache entry or its chunk storage to UI callers.
pub fn get_file_meta(asset_key: &str) -> Option<Arc<WaveformFileMeta>> {
    file_cache()
        .lock()
        .ok()?
        .get(asset_key)
        .and_then(|entry| entry.meta.clone())
}

/// The coarsest shipped LOD that still puts [`PEAKS_PER_COLUMN`] peaks behind
/// every pixel column.
///
/// # Why not the old rule
///
/// This used to allow a level up to **twice** as coarse as the pixel grid,
/// which is where the arrangement's blocky waveform came from: at 2× a single
/// stored peak spans two pixel columns, both columns draw the same min/max, and
/// the envelope becomes a staircase with every second edge invented. Worse, it
/// was doing that while the finer level was already sitting in the same peak
/// file — the detail was paid for at import and then not drawn.
///
/// Asking for four peaks per column instead means a column's min/max is a real
/// envelope of what happened under it rather than one stored pair, which is the
/// difference between a waveform that looks sampled and one that looks drawn.
/// The cost is bounded and small: a level `n` steps finer is `2^n` times more
/// peaks *per second of audio*, but the drawing only ever reads the peaks under
/// the visible columns, so the work per frame is the same — the columns are the
/// budget, not the file.
///
/// Nothing here can make the waveform disappear. When the chosen level is not
/// resident, [`best_available_samples_per_peak_in_entry`] falls back to whatever
/// the entry does have, and the canvas keeps a coarse fallback level for any
/// column the fine data has not reached yet.
pub fn pick_best_samples_per_peak(pixels_per_second: f32, sample_rate: u32) -> usize {
    let frames_per_pixel = (sample_rate as f32 / pixels_per_second.max(1.0))
        .round()
        .max(1.0) as usize;
    let target = (frames_per_pixel / PEAKS_PER_COLUMN).max(1);
    let mut best = LOD_LEVELS[0];
    for &spp in &LOD_LEVELS {
        if spp <= target {
            best = spp;
        }
    }
    best
}

/// Pick the closest LOD that is actually present in the locked cache entry.
///
/// During progressive import the fine primary chunks can be visible before the
/// full preview/LOD ladder is installed. Falling back here keeps partial
/// waveform rendering non-blocking without regenerating peaks in render.
pub(crate) fn best_available_samples_per_peak_in_entry(
    entry: &FileEntry,
    desired_samples_per_peak: usize,
) -> usize {
    if let Some(preview) = &entry.preview {
        if preview
            .lods
            .iter()
            .any(|lod| lod.samples_per_peak == desired_samples_per_peak)
        {
            return desired_samples_per_peak;
        }
    }

    if entry
        .chunks
        .keys()
        .any(|(spp, _)| *spp as usize == desired_samples_per_peak)
    {
        return desired_samples_per_peak;
    }

    let mut available: Vec<usize> = entry.chunks.keys().map(|(spp, _)| *spp as usize).collect();
    if let Some(preview) = &entry.preview {
        available.extend(preview.lods.iter().map(|lod| lod.samples_per_peak));
    }
    available.sort_unstable();
    available.dedup();

    available
        .iter()
        .copied()
        .filter(|spp| *spp <= desired_samples_per_peak.saturating_mul(2))
        .last()
        .or_else(|| available.first().copied())
        .or_else(|| entry.meta.as_ref().map(|meta| meta.primary_spp))
        .unwrap_or(desired_samples_per_peak)
}

pub fn get_import_state(path: &str) -> AudioImportState {
    file_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(path).map(|e| e.import.clone()))
        .unwrap_or(AudioImportState::Pending)
}

/// Returns false if import already running or file is ready.
pub fn try_begin_import(path_key: &str) -> bool {
    let Ok(mut cache) = file_cache().lock() else {
        return false;
    };
    if let Some(entry) = cache.get(path_key) {
        if entry.preview.is_some()
            || (entry.chunks_ready > 0 && entry.chunks_ready >= entry.chunks_total)
        {
            return false;
        }
        if matches!(
            entry.import,
            AudioImportState::Probing
                | AudioImportState::Decoding { .. }
                | AudioImportState::GeneratingPeaks { .. }
        ) {
            return false;
        }
    }
    cache.insert(
        path_key.to_string(),
        FileEntry {
            import: AudioImportState::Pending,
            meta: None,
            chunks_total: 0,
            chunks_ready: 0,
            chunks: HashMap::new(),
            preview: None,
            revision: 0,
        },
    );
    true
}

pub fn set_import_state(path_key: &str, state: AudioImportState) {
    if let Ok(mut cache) = file_cache().lock() {
        let entry = cache
            .entry(path_key.to_string())
            .or_insert_with(|| FileEntry {
                import: state.clone(),
                meta: None,
                chunks_total: 0,
                chunks_ready: 0,
                chunks: HashMap::new(),
                preview: None,
                revision: 0,
            });
        entry.import = state;
    }
}

pub fn begin_peak_build(path_key: &str, meta: Arc<WaveformFileMeta>, chunks_total: usize) {
    if let Ok(mut cache) = file_cache().lock() {
        let entry = cache
            .entry(path_key.to_string())
            .or_insert_with(|| FileEntry {
                import: AudioImportState::GeneratingPeaks { progress: 0.0 },
                meta: None,
                chunks_total: 0,
                chunks_ready: 0,
                chunks: HashMap::new(),
                preview: None,
                revision: 0,
            });
        entry.meta = Some(Arc::clone(&meta));
        entry.chunks_total = chunks_total;
        entry.chunks_ready = 0;
        entry.chunks.clear();
        entry.import = AudioImportState::GeneratingPeaks { progress: 0.0 };
        entry.revision = entry.revision.wrapping_add(1);
    }
}

pub fn install_chunk(
    path_key: &str,
    samples_per_peak: u32,
    chunk_index: u32,
    peaks: Arc<Vec<WaveformPeak>>,
) {
    if let Ok(mut cache) = file_cache().lock() {
        let Some(entry) = cache.get_mut(path_key) else {
            return;
        };
        entry.chunks.insert((samples_per_peak, chunk_index), peaks);
        entry.revision = entry.revision.wrapping_add(1);
        let primary = entry
            .meta
            .as_ref()
            .map(|m| m.primary_spp as u32)
            .unwrap_or(samples_per_peak);
        entry.chunks_ready = entry
            .chunks
            .keys()
            .filter(|(spp, _)| *spp == primary)
            .count();
        if entry.preview.is_none() {
            let progress = entry.chunks_ready as f32 / entry.chunks_total.max(1) as f32;
            entry.import = AudioImportState::GeneratingPeaks { progress };
        }
    }
}

/// Install an on-demand deep-zoom chunk (see [`super::waveform_detail`]).
///
/// Deliberately not [`install_chunk`]: that one is part of the *import*
/// progress model and recounts `chunks_ready` against the primary LOD, so
/// feeding it a finer level would report an import as more complete than it is.
/// A detail chunk only adds data and bumps the revision so the geometry cache
/// drops its now-too-coarse bars.
pub fn install_detail_chunk(
    path_key: &str,
    samples_per_peak: u32,
    chunk_index: u32,
    peaks: Arc<Vec<WaveformPeak>>,
) {
    if let Ok(mut cache) = file_cache().lock() {
        let Some(entry) = cache.get_mut(path_key) else {
            return;
        };
        entry.chunks.insert((samples_per_peak, chunk_index), peaks);
        entry.revision = entry.revision.wrapping_add(1);
    }
}

/// Drop one chunk. Used by the deep-zoom cache to stay inside its budget.
pub fn remove_chunk(path_key: &str, samples_per_peak: u32, chunk_index: u32) {
    if let Ok(mut cache) = file_cache().lock() {
        let Some(entry) = cache.get_mut(path_key) else {
            return;
        };
        if entry
            .chunks
            .remove(&(samples_per_peak, chunk_index))
            .is_some()
        {
            entry.revision = entry.revision.wrapping_add(1);
        }
    }
}

pub fn finish_peak_build(path_key: &str, preview: Arc<WaveformPreview>) {
    if let Ok(mut cache) = file_cache().lock() {
        let Some(entry) = cache.get_mut(path_key) else {
            return;
        };
        entry.preview = Some(preview);
        entry.import = AudioImportState::Ready;
        entry.revision = entry.revision.wrapping_add(1);
        if let Some(meta) = &entry.meta {
            entry.chunks_ready = entry.chunks_total.max(entry.chunks_ready);
            waveform_debug_log(&format!(
                "ready path={path_key} peaks={} chunks={}",
                meta.peak_count, entry.chunks_total
            ));
        }
    }
}

/// Split finest LOD into chunks and store (background thread only).
pub fn ingest_preview_as_chunks(path_key: &str, preview: Arc<WaveformPreview>) -> usize {
    let primary_lod = preview
        .lods
        .iter()
        .find(|l| l.samples_per_peak == PEAK_FINE_SPP)
        .or_else(|| preview.lods.first());
    let Some(primary_lod) = primary_lod else {
        return 0;
    };
    let meta = Arc::new(WaveformFileMeta {
        sample_rate: preview.sample_rate,
        channels: preview.channels,
        duration_seconds: preview.duration_seconds,
        total_frames: preview.total_frames,
        peak_count: primary_lod.peaks.len(),
        primary_spp: primary_lod.samples_per_peak,
    });
    let chunks_total = primary_lod.peaks.len().div_ceil(CHUNK_PEAKS);
    begin_peak_build(path_key, Arc::clone(&meta), chunks_total);
    for lod in &preview.lods {
        let spp = lod.samples_per_peak as u32;
        let lod_chunks_total = lod.peaks.len().div_ceil(CHUNK_PEAKS);
        for chunk_index in 0..lod_chunks_total {
            let start = chunk_index * CHUNK_PEAKS;
            let end = (start + CHUNK_PEAKS).min(lod.peaks.len());
            let slice = Arc::new(lod.peaks[start..end].to_vec());
            install_chunk(path_key, spp, chunk_index as u32, slice);
        }
    }
    finish_peak_build(path_key, preview);
    chunks_total
}

pub fn install_ready(path_key: &str, preview: Arc<WaveformPreview>) {
    ingest_preview_as_chunks(path_key, preview);
}

pub fn install_failed(path_key: &str, message: String) {
    if let Ok(mut cache) = file_cache().lock() {
        cache.insert(
            path_key.to_string(),
            FileEntry {
                import: AudioImportState::Failed {
                    message: message.clone(),
                },
                meta: None,
                chunks_total: 0,
                chunks_ready: 0,
                chunks: HashMap::new(),
                preview: None,
                revision: 0,
            },
        );
    }
}

/// Run `f` while holding the file cache lock once. Use this from render paths
/// instead of calling `aggregate_peak_range` per column (which re-locks).
pub(crate) fn with_file_entry<R>(path: &str, f: impl FnOnce(Option<&FileEntry>) -> R) -> R {
    let Ok(cache) = file_cache().lock() else {
        return f(None);
    };
    f(cache.get(path))
}

/// Display status from an already-locked entry (no extra lock).
pub(crate) fn display_status_from_entry(entry: &FileEntry) -> WaveformDisplayStatus {
    if let (Some(meta), true) = (&entry.meta, entry.preview.is_some()) {
        return WaveformDisplayStatus::Ready {
            meta: Arc::clone(meta),
        };
    }
    if let Some(meta) = &entry.meta {
        if entry.chunks_ready > 0 {
            return WaveformDisplayStatus::Partial {
                meta: Arc::clone(meta),
                chunks_ready: entry.chunks_ready,
                chunks_total: entry.chunks_total.max(1),
            };
        }
    }
    match &entry.import {
        AudioImportState::Failed { message } => WaveformDisplayStatus::Error(message.clone()),
        _ => WaveformDisplayStatus::Pending,
    }
}

/// Aggregate min/max for peak indices `[peak_start, peak_end)` on a locked entry.
pub(crate) fn aggregate_peak_range_in_entry(
    entry: &FileEntry,
    samples_per_peak: usize,
    peak_start: usize,
    peak_end: usize,
) -> WaveformPeak {
    aggregate_peak_range_in_entry_opt(entry, samples_per_peak, peak_start, peak_end)
        .unwrap_or(WaveformPeak { min: 0.0, max: 0.0 })
}

/// The same aggregate, distinguishing **no data** from **silence**.
///
/// `None` means this level holds nothing for that range — the peaks were never
/// built, or the deep-zoom chunk covering it is not resident. `Some` with a
/// zero min/max means the audio there really is silent.
///
/// The two used to be the same answer, and that is what made a waveform vanish
/// when it was zoomed in far enough to leave the shipped ladder behind: the
/// fine chunks are decoded in the background, so the first frames after a zoom
/// have none, every column aggregated to zero, and the drawing skipped every
/// bar instead of falling back to the coarse peaks it already had in memory.
pub(crate) fn aggregate_peak_range_in_entry_opt(
    entry: &FileEntry,
    samples_per_peak: usize,
    peak_start: usize,
    peak_end: usize,
) -> Option<WaveformPeak> {
    if let Some(preview) = &entry.preview {
        if let Some(lod) = preview
            .lods
            .iter()
            .find(|l| l.samples_per_peak == samples_per_peak)
        {
            let end = peak_end.min(lod.peaks.len());
            let start = peak_start.min(end);
            if start >= end {
                // Past the end of a level that exists: nothing to draw here,
                // and nothing a coarser level would add either.
                return Some(WaveformPeak { min: 0.0, max: 0.0 });
            }
            return Some(aggregate_slice(&lod.peaks[start..end]));
        }
    }

    let spp = samples_per_peak as u32;
    aggregate_chunked(peak_start, peak_end, |chunk| {
        entry
            .chunks
            .get(&(spp, chunk as u32))
            .map(|peaks| peaks.as_slice())
    })
}

/// Min/max over peak indices `[peak_start, peak_end)` of a chunked level,
/// `None` when no chunk in the range holds any of them.
///
/// Walks the range a chunk at a time — one `chunk` lookup per [`CHUNK_PEAKS`]
/// peaks rather than one per peak, which is what a zoomed-out column spanning
/// thousands of peaks used to pay in hash lookups.
fn aggregate_chunked<'a>(
    peak_start: usize,
    peak_end: usize,
    chunk: impl Fn(usize) -> Option<&'a [WaveformPeak]>,
) -> Option<WaveformPeak> {
    let mut mn = f32::MAX;
    let mut mx = f32::MIN;
    let mut any = false;
    let mut pk = peak_start;
    while pk < peak_end {
        let ci = pk / CHUNK_PEAKS;
        let base = ci * CHUNK_PEAKS;
        let next = (base + CHUNK_PEAKS).min(peak_end);
        if let Some(peaks) = chunk(ci) {
            let end = (next - base).min(peaks.len());
            let start = pk - base;
            if start < end {
                for p in &peaks[start..end] {
                    if p.min < mn {
                        mn = p.min;
                    }
                    if p.max > mx {
                        mx = p.max;
                    }
                }
                any = true;
            }
        }
        pk = next;
    }
    any.then_some(WaveformPeak { min: mn, max: mx })
}

/// One peak level's data over a span of peak indices, held by `Arc` so it can
/// be aggregated after the file-cache lock is released.
///
/// The render path used to run its whole per-column aggregation inside
/// [`with_file_entry`], holding the global file cache for as long as a clip's
/// bars took to build — and every background chunk install queued behind it.
/// Taking this snapshot costs one `Arc` clone per chunk; the aggregation then
/// runs unlocked.
pub(crate) enum PeakSpan {
    /// A level of the preview ladder: built for the whole file.
    Ladder {
        preview: Arc<WaveformPreview>,
        lod: usize,
    },
    /// A chunked level: the chunks covering the span, `None` where a chunk
    /// is not resident.
    Chunks {
        first_chunk: usize,
        chunks: Vec<Option<Arc<Vec<WaveformPeak>>>>,
    },
}

/// Snapshot `samples_per_peak` over peak indices `[peak_start, peak_end)`.
/// [`PeakSpan::aggregate`] answers exactly what
/// [`aggregate_peak_range_in_entry_opt`] would on this entry for any range
/// inside that span.
pub(crate) fn peak_span_in_entry(
    entry: &FileEntry,
    samples_per_peak: usize,
    peak_start: usize,
    peak_end: usize,
) -> PeakSpan {
    if let Some(preview) = &entry.preview {
        if let Some(lod) = preview
            .lods
            .iter()
            .position(|l| l.samples_per_peak == samples_per_peak)
        {
            return PeakSpan::Ladder {
                preview: Arc::clone(preview),
                lod,
            };
        }
    }
    if peak_end <= peak_start {
        return PeakSpan::Chunks {
            first_chunk: 0,
            chunks: Vec::new(),
        };
    }
    let spp = samples_per_peak as u32;
    let first_chunk = peak_start / CHUNK_PEAKS;
    let last_chunk = (peak_end - 1) / CHUNK_PEAKS;
    PeakSpan::Chunks {
        first_chunk,
        chunks: (first_chunk..=last_chunk)
            .map(|chunk| entry.chunks.get(&(spp, chunk as u32)).cloned())
            .collect(),
    }
}

impl PeakSpan {
    /// [`aggregate_peak_range_in_entry_opt`] over the snapshot. A range
    /// reaching outside the snapshotted span reads as not resident there.
    pub(crate) fn aggregate(&self, peak_start: usize, peak_end: usize) -> Option<WaveformPeak> {
        match self {
            Self::Ladder { preview, lod } => {
                let peaks = &preview.lods[*lod].peaks;
                let end = peak_end.min(peaks.len());
                let start = peak_start.min(end);
                if start >= end {
                    return Some(WaveformPeak { min: 0.0, max: 0.0 });
                }
                Some(aggregate_slice(&peaks[start..end]))
            }
            Self::Chunks {
                first_chunk,
                chunks,
            } => aggregate_chunked(peak_start, peak_end, |chunk| {
                chunks
                    .get(chunk.checked_sub(*first_chunk)?)?
                    .as_deref()
                    .map(Vec::as_slice)
            }),
        }
    }
}

/// Whether every chunk covering peak indices `[peak_start, peak_end)` is
/// resident at `samples_per_peak`.
///
/// A level that only half covers the visible span is worse than the coarse
/// ladder: the covered columns come out at one resolution and the rest fall
/// back to another, and a min/max envelope is taller the coarser it is, so the
/// seam shows as a step in the waveform. Asking this first lets the drawing
/// commit to one level for the whole frame and sharpen when the rest lands.
///
/// A level that lives in the preview ladder is always fully resident — it is
/// built for the whole file — so it answers `true` without touching chunks.
pub(crate) fn chunks_cover_peak_range(
    entry: &FileEntry,
    samples_per_peak: usize,
    peak_start: usize,
    peak_end: usize,
) -> bool {
    if let Some(preview) = &entry.preview {
        if preview
            .lods
            .iter()
            .any(|lod| lod.samples_per_peak == samples_per_peak)
        {
            return true;
        }
    }
    if peak_end <= peak_start {
        return true;
    }
    let spp = samples_per_peak as u32;
    let first = peak_start / CHUNK_PEAKS;
    let last = (peak_end - 1) / CHUNK_PEAKS;
    (first..=last).all(|chunk| entry.chunks.contains_key(&(spp, chunk as u32)))
}

/// The coarsest peak level this entry actually holds for the whole file.
///
/// Used as the fallback while finer, chunked detail is still being decoded: the
/// shipped ladder is built with the preview and is always resident, so there is
/// always something to draw.
pub(crate) fn resident_ladder_samples_per_peak(entry: &FileEntry) -> Option<usize> {
    entry
        .preview
        .as_ref()
        .and_then(|preview| preview.lods.first())
        .map(|lod| lod.samples_per_peak)
}

/// Aggregate min/max for peak indices `[start, end)` using chunked storage.
pub fn aggregate_peak_range(
    path: &str,
    samples_per_peak: usize,
    peak_start: usize,
    peak_end: usize,
) -> WaveformPeak {
    with_file_entry(path, |entry| {
        entry
            .map(|e| aggregate_peak_range_in_entry(e, samples_per_peak, peak_start, peak_end))
            .unwrap_or(WaveformPeak { min: 0.0, max: 0.0 })
    })
}

fn aggregate_slice(peaks: &[WaveformPeak]) -> WaveformPeak {
    if peaks.is_empty() {
        return WaveformPeak { min: 0.0, max: 0.0 };
    }
    let mut mn = peaks[0].min;
    let mut mx = peaks[0].max;
    for p in &peaks[1..] {
        if p.min < mn {
            mn = p.min;
        }
        if p.max > mx {
            mx = p.max;
        }
    }
    WaveformPeak { min: mn, max: mx }
}

fn waveform_debug_log(msg: &str) {
    if std::env::var_os("FUTUREBOARD_WAVEFORM_DEBUG").is_some() {
        eprintln!("[waveform] {msg}");
    }
}

/// Drop in-memory peaks for `path_key` so the next import decodes the file again.
///
/// Required when a clip's source is rewritten in place (same path) or retargeted
/// onto a new derived file that must not keep drawing the previous asset.
pub fn invalidate_file(path_key: &str) {
    if let Ok(mut cache) = file_cache().lock() {
        cache.remove(path_key);
    }
    if let Ok(mut cache) = clip_cache().lock() {
        cache.remove(path_key);
    }
}

/// Legacy entry: marks pending; actual work is started by `audio_import::start_file_import`.
pub fn request_decode_file(path: PathBuf) {
    let key = path.to_string_lossy().to_string();
    let _ = try_begin_import(&key);
}

/// Background-only peak decode. Prefer `audio_import` pipeline.
pub fn decode_and_cache_file(path: &Path) -> Option<Arc<WaveformPreview>> {
    let key = path.to_string_lossy().to_string();
    if let Some(arc) = get_preview_arc(&key) {
        return Some(arc);
    }
    let preview = decode_file_uncached(path)?;
    let arc = Arc::new(preview);
    install_ready(&key, Arc::clone(&arc));
    Some(arc)
}

fn decode_file_uncached(path: &Path) -> Option<WaveformPreview> {
    match generate_audio_peaks(path) {
        Ok(peaks) => Some(peaks.into()),
        Err(error) => {
            eprintln!(
                "[waveform] generate_audio_peaks failed: path={} error={}",
                path.display(),
                error
            );
            None
        }
    }
}

// ── Realtime recording preview registry (Part 1) ──────────────────────────────
//
// Live takes are drawn from a side registry keyed by the temporary preview
// clip's id, so the normal file/placeholder clip pipeline is untouched. The
// audio poll rebuilds the `WaveformPreview` as peaks stream in; the render path
// only reads it.

fn recording_preview_registry() -> &'static Mutex<HashMap<String, Arc<WaveformPreview>>> {
    static REG: OnceLock<Mutex<HashMap<String, Arc<WaveformPreview>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Publish (or replace) the live preview for `clip_id`.
pub fn set_recording_preview(clip_id: &str, preview: Arc<WaveformPreview>) {
    recording_preview_registry()
        .lock()
        .unwrap()
        .insert(clip_id.to_string(), preview);
}

/// Read the live preview for `clip_id`, if a take is currently feeding it.
pub fn recording_preview(clip_id: &str) -> Option<Arc<WaveformPreview>> {
    recording_preview_registry()
        .lock()
        .unwrap()
        .get(clip_id)
        .map(Arc::clone)
}

/// Drop the live preview for `clip_id` (take finished / cancelled).
pub fn clear_recording_preview(clip_id: &str) {
    recording_preview_registry().lock().unwrap().remove(clip_id);
}

/// Build a single-LOD [`WaveformPreview`] from streamed min/max/rms peak bins.
pub fn preview_from_recording_peaks(
    peaks: &[WaveformPeak],
    sample_rate: u32,
    peaks_per_second: u32,
) -> WaveformPreview {
    let pps = peaks_per_second.max(1);
    let samples_per_peak = (sample_rate.max(1) / pps).max(1) as usize;
    let duration_seconds = peaks.len() as f64 / pps as f64;
    WaveformPreview {
        sample_rate,
        channels: 1,
        duration_seconds,
        total_frames: (peaks.len() * samples_per_peak) as u64,
        lods: vec![WaveformLod {
            samples_per_peak,
            peaks: peaks.to_vec(),
        }],
    }
}

// ── Demo / placeholder previews ───────────────────────────────────────────────

pub fn get_or_generate_waveform(
    clip_id: &str,
    name: &str,
    duration_beats: f32,
    bpm: f32,
) -> Arc<WaveformPreview> {
    let mut cache = clip_cache().lock().unwrap();
    if let Some(preview) = cache.get(clip_id) {
        return Arc::clone(preview);
    }
    let preview = Arc::new(generate_waveform_preview(name, duration_beats, bpm));
    cache.insert(clip_id.to_string(), Arc::clone(&preview));
    preview
}

pub fn placeholder_waveform(duration_seconds: f64) -> WaveformPreview {
    let lod = WaveformLod {
        samples_per_peak: LOD_LEVELS[LOD_LEVELS.len() / 2],
        peaks: (0..32)
            .map(|_| WaveformPeak {
                min: -0.03,
                max: 0.03,
            })
            .collect(),
    };
    WaveformPreview {
        sample_rate: 44_100,
        channels: 1,
        duration_seconds,
        total_frames: (duration_seconds * 44_100.0) as u64,
        lods: vec![lod],
    }
}

fn generate_waveform_preview(name: &str, duration_beats: f32, bpm: f32) -> WaveformPreview {
    let sample_rate: u32 = 44_100;
    let duration_seconds = duration_beats as f64 * (60.0 / bpm.max(1.0) as f64);
    let total_samples = (duration_seconds * sample_rate as f64) as u64;
    let total_samples = total_samples.min(sample_rate as u64 * 30);

    let mut lods = LodSet::new(total_samples);
    let name_lower = name.to_lowercase();
    let kind = if name_lower.contains("drum") || name_lower.contains("loop") {
        SynthKind::Drums
    } else if name_lower.contains("vocal")
        || name_lower.contains("harmony")
        || name_lower.contains("dry")
    {
        SynthKind::Vocals
    } else {
        SynthKind::Generic
    };

    for i in 0..total_samples as usize {
        let t = i as f32 / sample_rate as f32;
        let v = synth_sample(kind, t, duration_seconds as f32);
        lods.push(v.clamp(-1.0, 1.0));
    }

    WaveformPreview {
        sample_rate,
        channels: 1,
        duration_seconds,
        total_frames: total_samples,
        lods: lods.finalize(),
    }
}

struct LodBuilder {
    samples_per_peak: usize,
    min: f32,
    max: f32,
    count: usize,
    peaks: Vec<WaveformPeak>,
}

impl LodBuilder {
    fn new(samples_per_peak: usize, total_samples_hint: u64) -> Self {
        let cap = (total_samples_hint as usize / samples_per_peak).saturating_add(1);
        Self {
            samples_per_peak,
            min: 0.0,
            max: 0.0,
            count: 0,
            peaks: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, v: f32) {
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
        }
        self.count += 1;
        if self.count >= self.samples_per_peak {
            self.peaks.push(WaveformPeak {
                min: self.min,
                max: self.max,
            });
            self.min = 0.0;
            self.max = 0.0;
            self.count = 0;
        }
    }

    fn finalize(mut self) -> WaveformLod {
        if self.count > 0 {
            self.peaks.push(WaveformPeak {
                min: self.min,
                max: self.max,
            });
        }
        WaveformLod {
            samples_per_peak: self.samples_per_peak,
            peaks: self.peaks,
        }
    }
}

struct LodSet {
    builders: Vec<LodBuilder>,
}

impl LodSet {
    fn new(total_samples_hint: u64) -> Self {
        Self {
            builders: LOD_LEVELS
                .iter()
                .map(|&spp| LodBuilder::new(spp, total_samples_hint))
                .collect(),
        }
    }

    fn push(&mut self, mono: f32) {
        for b in &mut self.builders {
            b.push(mono);
        }
    }

    fn finalize(self) -> Vec<WaveformLod> {
        self.builders
            .into_iter()
            .map(LodBuilder::finalize)
            .collect()
    }
}

#[derive(Copy, Clone)]
enum SynthKind {
    Drums,
    Vocals,
    Generic,
}

fn synth_sample(kind: SynthKind, t: f32, duration: f32) -> f32 {
    let beat = t * 2.0;
    match kind {
        SynthKind::Drums => {
            let beat_fract = beat.fract();
            let beat_int = beat.floor() as i32;
            let mut amp = 0.04;
            if beat_int % 4 == 0 {
                amp += 0.82 * (-6.0 * beat_fract).exp();
            }
            if beat_int % 4 == 2 {
                amp += 0.68 * (-4.5 * beat_fract).exp();
            }
            let hat_pos = (beat * 2.0).fract();
            if hat_pos < 0.12 {
                amp += 0.28 * (-14.0 * hat_pos).exp();
            }
            let noise = ((t * 83.19).sin() * 43758.5453).fract() - 0.5;
            (amp + noise * 0.05).clamp(-1.0, 1.0)
                * if (t * 1000.0).sin() > 0.0 { 1.0 } else { -1.0 }
        }
        SynthKind::Vocals => {
            let phrase_beat = beat % 4.0;
            let mut amp = 0.0;
            if phrase_beat < 3.0 {
                amp = (phrase_beat * std::f32::consts::PI / 3.0).sin() * 0.58;
                amp += (beat * 7.5).sin().abs() * 0.14;
                amp += (beat * 22.0).sin().abs() * 0.06;
            }
            let osc = (t * 440.0 * 2.0 * std::f32::consts::PI).sin();
            (amp * osc).clamp(-1.0, 1.0)
        }
        SynthKind::Generic => {
            let env = (t / duration.max(0.001)).clamp(0.0, 1.0);
            let osc = (t * 220.0 * 2.0 * std::f32::consts::PI).sin();
            (osc * (0.4 + 0.2 * (t * 3.0).sin()) * (1.0 - env * 0.3)).clamp(-1.0, 1.0)
        }
    }
}

// ── Render geometry cache ─────────────────────────────────────────────────────
//
// The render path turns peaks into one min/max bar per visible pixel column.
// That per-column aggregation used to re-run (and re-allocate) on *every*
// timeline repaint — including every playhead tick and meter notify — even when
// nothing about a clip's geometry changed. This cache stores the finished bar
// list keyed by a hash of everything that affects the geometry (peak revision,
// LOD, column tile, clip size, stretch/reverse). On a hit the render path
// reuses an immutable `Arc<WaveformBars>` and skips both the aggregation and the
// allocation; on a miss it computes once and publishes. Bars hold no color, so
// two clips with identical geometry share one entry.

/// `(x_in_canvas, top, height)` per drawn column.
/// Cached per-column waveform geometry: `(x, min_peak, max_peak)` in `[-1, 1]`.
/// Vertical pixel placement is derived from the paint bounds at draw time.
pub type WaveformBars = Vec<(f32, f32, f32)>;

/// FIFO-bounded so memory stays flat (clips × zoom states cycle through). Not
/// strict LRU — insertion-order eviction is enough because stale geometries
/// (old scroll/zoom/revision) are never looked up again and fall off the back.
struct GeometryCache {
    map: HashMap<u64, Arc<WaveformBars>>,
    order: VecDeque<u64>,
    cap: usize,
}

impl GeometryCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn get(&self, key: u64) -> Option<Arc<WaveformBars>> {
        self.map.get(&key).cloned()
    }

    fn insert(&mut self, key: u64, bars: Arc<WaveformBars>) {
        if self.map.insert(key, bars).is_none() {
            self.order.push_back(key);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

static GEOMETRY_CACHE: OnceLock<Mutex<GeometryCache>> = OnceLock::new();

fn geometry_cache() -> &'static Mutex<GeometryCache> {
    // Entries are fixed-width column tiles (see `waveform_canvas`), a few KB
    // each, and a visible clip holds several. 4k keeps memory bounded at
    // roughly what 1k viewport-wide bar lists used to cost while still far
    // exceeding the live working set (visible clips × their tiles × a handful
    // of recent zoom states).
    GEOMETRY_CACHE.get_or_init(|| Mutex::new(GeometryCache::new(4096)))
}

/// Look up precomputed bars for a geometry signature (see [`WaveformBars`]).
pub fn geometry_cache_get(key: u64) -> Option<Arc<WaveformBars>> {
    geometry_cache().lock().ok()?.get(key)
}

/// Publish precomputed bars for a geometry signature.
pub fn geometry_cache_put(key: u64, bars: Arc<WaveformBars>) {
    if let Ok(mut cache) = geometry_cache().lock() {
        cache.insert(key, bars);
    }
}

pub fn pick_lod<'a>(
    preview: &'a WaveformPreview,
    samples_per_pixel: f32,
) -> Option<&'a WaveformLod> {
    if preview.lods.is_empty() {
        return None;
    }
    let target = samples_per_pixel.max(1.0);
    let mut best: &WaveformLod = &preview.lods[0];
    for lod in &preview.lods {
        if lod.samples_per_peak as f32 <= target {
            best = lod;
        } else {
            break;
        }
    }
    Some(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `FileEntry` holding the shipped ladder and, optionally, some deep-zoom
    /// chunks — the two states the drawing has to tell apart.
    fn entry_with(ladder: Option<usize>, chunks: &[(u32, u32)]) -> FileEntry {
        let preview = ladder.map(|samples_per_peak| {
            Arc::new(WaveformPreview {
                sample_rate: 48_000,
                channels: 2,
                duration_seconds: 1.0,
                total_frames: 48_000,
                lods: vec![WaveformLod {
                    samples_per_peak,
                    peaks: vec![
                        WaveformPeak {
                            min: -0.5,
                            max: 0.5
                        };
                        8
                    ],
                }],
            })
        });
        let mut entry = FileEntry {
            import: AudioImportState::default(),
            meta: None,
            chunks_total: 0,
            chunks_ready: 0,
            chunks: HashMap::new(),
            preview,
            revision: 0,
        };
        for &(spp, chunk) in chunks {
            entry.chunks.insert(
                (spp, chunk),
                Arc::new(vec![
                    WaveformPeak {
                        min: -0.25,
                        max: 0.25
                    };
                    CHUNK_PEAKS
                ]),
            );
        }
        entry
    }

    /// The bug this covers: zoom in past the shipped ladder and the fine peaks
    /// are still being decoded, so every column aggregates to nothing. Reported
    /// as `Some(0, 0)` that is indistinguishable from silence, which is why the
    /// drawing skipped every bar and the waveform vanished — while zooming out,
    /// back onto the ladder, looked perfectly normal.
    #[test]
    fn a_level_with_no_data_is_not_reported_as_silence() {
        let entry = entry_with(Some(256), &[]);

        // The ladder has data: a real answer, even where the audio is quiet.
        let ladder =
            aggregate_peak_range_in_entry_opt(&entry, 256, 0, 4).expect("the ladder has data");
        assert_eq!((ladder.min, ladder.max), (-0.5, 0.5));
        // A finer level nobody has decoded yet has *nothing*, and must say so
        // rather than answering zero.
        assert!(aggregate_peak_range_in_entry_opt(&entry, 4, 0, 4).is_none());
        // The old entry point keeps its shape for callers that cannot act on
        // the difference.
        let flattened = aggregate_peak_range_in_entry(&entry, 4, 0, 4);
        assert_eq!((flattened.min, flattened.max), (0.0, 0.0));
    }

    /// The chunk-at-a-time walk against the one-lookup-per-peak loop it
    /// replaced, over ranges that start and end mid-chunk, span several
    /// chunks, cross missing ones and run past a short last chunk — and the
    /// unlocked snapshot against both.
    #[test]
    fn chunked_aggregation_matches_a_per_peak_walk() {
        let spp = 64usize;
        let mut entry = entry_with(Some(256), &[]);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for chunk in [0u32, 1, 3, 4] {
            // Chunk 4 is short, as a file's last chunk is.
            let len = if chunk == 4 { 1000 } else { CHUNK_PEAKS };
            let peaks = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    WaveformPeak {
                        min: -((seed & 0xFFFF) as f32) / 65_535.0,
                        max: ((seed >> 16) & 0xFFFF) as f32 / 65_535.0,
                    }
                })
                .collect();
            entry.chunks.insert((spp as u32, chunk), Arc::new(peaks));
        }
        let per_peak = |start: usize, end: usize| {
            let mut found: Option<WaveformPeak> = None;
            for pk in start..end {
                let chunk = entry.chunks.get(&(spp as u32, (pk / CHUNK_PEAKS) as u32));
                if let Some(p) = chunk.and_then(|c| c.get(pk % CHUNK_PEAKS)) {
                    let f = found.get_or_insert(*p);
                    f.min = f.min.min(p.min);
                    f.max = f.max.max(p.max);
                }
            }
            found.map(|p| (p.min, p.max))
        };
        let c = CHUNK_PEAKS;
        let ranges = [
            (0, 1),
            (5, 5),
            (17, 3_000),
            (c - 3, c + 3),
            (100, 3 * c + 50),
            (2 * c, 3 * c),
            (2 * c + 10, 2 * c + 20),
            (3 * c - 1, 4 * c + 999),
            (4 * c + 990, 4 * c + 1_010),
            (4 * c + 1_000, 5 * c),
            (0, 6 * c),
        ];
        for (start, end) in ranges {
            let expected = per_peak(start, end);
            let got = aggregate_peak_range_in_entry_opt(&entry, spp, start, end);
            assert_eq!(
                got.map(|p| (p.min, p.max)),
                expected,
                "range {start}..{end}"
            );
            let span = peak_span_in_entry(&entry, spp, start, end);
            let snap = span.aggregate(start, end);
            assert_eq!(
                snap.map(|p| (p.min, p.max)),
                expected,
                "snapshot {start}..{end}"
            );
        }
        // A ladder level snapshots as the ladder and answers as it does.
        let ladder = peak_span_in_entry(&entry, 256, 0, 4);
        assert!(matches!(ladder, PeakSpan::Ladder { .. }));
        for (start, end) in [(0, 4), (2, 3), (900, 950)] {
            assert_eq!(
                ladder.aggregate(start, end).map(|p| (p.min, p.max)),
                aggregate_peak_range_in_entry_opt(&entry, 256, start, end).map(|p| (p.min, p.max))
            );
        }
    }

    /// Past the end of a level that does exist is a real answer: there is no
    /// audio there and no coarser level would find any.
    #[test]
    fn past_the_end_of_a_real_level_is_silence_not_missing_data() {
        let entry = entry_with(Some(256), &[]);
        let past = aggregate_peak_range_in_entry_opt(&entry, 256, 900, 950)
            .expect("a level that exists always answers");
        assert_eq!((past.min, past.max), (0.0, 0.0));
    }

    /// The fallback the drawing leans on has to exist whenever a preview does,
    /// or a deep zoom has nothing to fall back to.
    #[test]
    fn the_resident_ladder_is_whatever_the_preview_shipped() {
        assert_eq!(
            resident_ladder_samples_per_peak(&entry_with(Some(512), &[])),
            Some(512)
        );
        assert_eq!(
            resident_ladder_samples_per_peak(&entry_with(None, &[])),
            None
        );
    }

    /// Half a visible span at one resolution and half at another shows as a
    /// step, because a min/max envelope is taller the coarser it is. The
    /// drawing asks this so it can commit to one level for the whole frame.
    #[test]
    fn a_partly_decoded_level_does_not_cover_the_span() {
        // Chunk 0 only, at 4 samples per peak.
        let entry = entry_with(Some(256), &[(4, 0)]);

        assert!(chunks_cover_peak_range(&entry, 4, 0, CHUNK_PEAKS));
        assert!(
            !chunks_cover_peak_range(&entry, 4, 0, CHUNK_PEAKS + 1),
            "a span reaching into the chunk after it is not covered"
        );
        assert!(!chunks_cover_peak_range(
            &entry,
            4,
            CHUNK_PEAKS,
            CHUNK_PEAKS * 2
        ));
        // A ladder level is built for the whole file, so it always covers.
        assert!(chunks_cover_peak_range(&entry, 256, 0, 10_000_000));
        // An empty span asks nothing and is trivially covered.
        assert!(chunks_cover_peak_range(&entry, 4, 5, 5));
    }

    #[test]
    fn lod_selection_tracks_zoom() {
        let sr = 48_000;
        // Zoomed out (few px/s) → coarse LOD; zoomed in (thousands px/s) → finest.
        let coarse = pick_best_samples_per_peak(5.0, sr);
        let fine = pick_best_samples_per_peak(5_000.0, sr);
        // Coarse in proportion to the zoom, not to a fixed floor: the picker
        // now aims for peaks-per-column, so what counts as coarse moves with
        // `PEAKS_PER_COLUMN` rather than being a number to memorise.
        assert!(
            coarse >= 1024,
            "zoomed-out should pick a coarse LOD, got {coarse}"
        );
        assert_eq!(fine, LOD_LEVELS[0], "zoomed-in should pick the finest LOD");
        // Zooming in must never select a coarser LOD than zooming out.
        assert!(fine <= coarse);
    }

    /// The stair-step regression, stated as a rule: a pixel column must never
    /// be drawn from fewer than one stored peak, and the picker aims for
    /// several. A level coarser than the pixel grid is what made neighbouring
    /// columns repeat each other's min/max.
    #[test]
    fn every_column_gets_at_least_one_peak() {
        let sr = 48_000;
        for pps in [5.0, 25.0, 60.0, 100.0, 240.0, 600.0, 1_500.0] {
            let frames_per_pixel = sr as f32 / pps;
            let spp = pick_best_samples_per_peak(pps, sr);
            assert!(
                (spp as f32) <= frames_per_pixel || spp == LOD_LEVELS[0],
                "at {pps} px/s a column spans {frames_per_pixel} frames but the \
                 picker chose {spp} frames per peak, so one peak covers more than \
                 one column"
            );
        }
    }

    #[test]
    fn lod_selection_always_returns_a_real_level() {
        let sr = 48_000;
        for pps in [1.0, 10.0, 50.0, 250.0, 1_000.0, 8_000.0] {
            let spp = pick_best_samples_per_peak(pps, sr);
            assert!(
                LOD_LEVELS.contains(&spp),
                "spp {spp} is not a real LOD level (pps={pps})"
            );
        }
    }

    #[test]
    fn pick_lod_prefers_coarsest_within_budget() {
        let preview = WaveformPreview {
            sample_rate: 48_000,
            channels: 1,
            duration_seconds: 1.0,
            total_frames: 48_000,
            lods: LOD_LEVELS
                .iter()
                .map(|&spp| WaveformLod {
                    samples_per_peak: spp,
                    peaks: vec![WaveformPeak {
                        min: -0.5,
                        max: 0.5,
                    }],
                })
                .collect(),
        };
        // budget 1000 spp → coarsest level whose spp ≤ 1000 is 512.
        assert_eq!(pick_lod(&preview, 1000.0).unwrap().samples_per_peak, 512);
        // budget below the finest level → finest available.
        assert_eq!(
            pick_lod(&preview, 1.0).unwrap().samples_per_peak,
            LOD_LEVELS[0]
        );
    }

    #[test]
    fn geometry_cache_roundtrips() {
        let bars: Arc<WaveformBars> = Arc::new(vec![(1.0, 2.0, 3.0)]);
        let key = 0x5151_5151_5151_5151;
        geometry_cache_put(key, Arc::clone(&bars));
        let got = geometry_cache_get(key).expect("cached bars present");
        assert_eq!(got.as_slice(), &[(1.0, 2.0, 3.0)]);
        assert!(geometry_cache_get(0x0123_4567_89AB_CDEF).is_none());
    }

    #[test]
    fn geometry_cache_evicts_fifo_when_over_cap() {
        let mut cache = GeometryCache::new(2);
        cache.insert(1, Arc::new(vec![]));
        cache.insert(2, Arc::new(vec![]));
        cache.insert(3, Arc::new(vec![])); // pushes 1 off the back
        assert!(cache.get(1).is_none(), "oldest entry should be evicted");
        assert!(cache.get(2).is_some());
        assert!(cache.get(3).is_some());
    }

    #[test]
    fn invalidate_file_drops_the_cached_entry() {
        let key = "test-invalidate-asset";
        begin_peak_build(
            key,
            Arc::new(WaveformFileMeta {
                sample_rate: 48_000,
                channels: 1,
                duration_seconds: 1.0,
                total_frames: 48_000,
                primary_spp: 256,
                peak_count: 1,
            }),
            1,
        );
        assert!(get_file_meta(key).is_some());
        invalidate_file(key);
        assert!(get_file_meta(key).is_none());
    }
}
